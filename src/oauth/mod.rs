//! The `oauth` service: the Rust twin of `OAuthIntegration` (`@almadar/integrations` `integrations/oauth`).
//! Two backends behind one contract: the OIDC backend (authorization code + PKCE against a discovered issuer or
//! GitHub's static OAuth 2 metadata, the twin of `openid-client`) when `OAUTH_CLIENT_ID` + `OAUTH_CLIENT_SECRET`
//! are configured and `OAUTH_MODE` is not `mock`, and the in-memory mock backend otherwise.

mod connections;
mod mock;
mod oidc;
mod zod;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

pub use self::connections::{ConnectionRow, OAuthConnectionStore};
use self::mock::MockBackend;
use self::oidc::{OidcClient, OidcServer, Pending, Tokens};
use crate::credentials::Credentials;
use crate::error::{IntegrationError, NOT_CONFIGURED};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::Transport;
use crate::wallclock::{system_clock, Clock};

pub const SERVICE: &str = "oauth";

pub(crate) const PENDING_GRANT_TTL_MS: i64 = 10 * 60 * 1000;
const AUTH_ERROR: &str = "AUTH_ERROR";
const GOOGLE_ISSUER: &str = "https://accounts.google.com";

type Rng = Arc<dyn Fn(&mut [u8]) + Send + Sync>;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OAuthProviderKind {
    Google,
    Github,
    Auth0,
}

impl OAuthProviderKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Github => "github",
            Self::Auth0 => "auth0",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "google" => Some(Self::Google),
            "github" => Some(Self::Github),
            "auth0" => Some(Self::Auth0),
            _ => None,
        }
    }

    /// Default OIDC issuer per provider (Google Workspace first).
    fn issuer(self) -> Option<&'static str> {
        match self {
            Self::Google => Some(GOOGLE_ISSUER),
            _ => None,
        }
    }

    /// Plain OAuth 2 providers with no discovery document.
    fn static_metadata(self) -> Option<Map<String, Value>> {
        match self {
            Self::Github => match json!({
                "issuer": "https://github.com",
                "authorization_endpoint": "https://github.com/login/oauth/authorize",
                "token_endpoint": "https://github.com/login/oauth/access_token",
            }) {
                Value::Object(map) => Some(map),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Whether the credentials configure `oauth` at all (`RuntimeIntegrationManager`).
pub fn is_configured(creds: &Credentials) -> bool {
    (creds.get("OAUTH_CLIENT_ID").is_some() && creds.get("OAUTH_CLIENT_SECRET").is_some())
        || creds.get("OAUTH_MODE") == Some("mock")
}

/// Register `oauth` on `integrations`: ready when configured, unconfigured otherwise.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    if is_configured(creds) {
        integrations.register(SERVICE, Arc::new(OauthProvider::new(creds, transport)));
    } else {
        integrations.register_unconfigured(SERVICE);
    }
}

struct Config {
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    issuer_url: String,
}

pub struct OauthProvider {
    config: Config,
    real: bool,
    transport: Arc<dyn Transport>,
    clock: Clock,
    rng: Rng,
    mock: Mutex<MockBackend>,
    clients: Mutex<HashMap<OAuthProviderKind, Arc<OidcClient>>>,
    pending: Mutex<HashMap<String, (Pending, i64)>>,
    subjects: Mutex<HashMap<String, String>>,
    connections: Option<Arc<dyn OAuthConnectionStore>>,
}

impl OauthProvider {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        let text = |name: &str| creds.get(name).unwrap_or_default().to_string();
        let config = Config {
            client_id: text("OAUTH_CLIENT_ID"),
            client_secret: text("OAUTH_CLIENT_SECRET"),
            redirect_uri: text("OAUTH_REDIRECT_URI"),
            issuer_url: text("OIDC_ISSUER_URL"),
        };
        let real = creds.get("OAUTH_MODE") != Some("mock")
            && !config.client_id.is_empty()
            && !config.client_secret.is_empty();
        Self {
            config,
            real,
            transport,
            clock: system_clock(),
            rng: Arc::new(|buf| {
                getrandom::getrandom(buf).expect("the operating system provides randomness");
            }),
            mock: Mutex::new(MockBackend::default()),
            clients: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            subjects: Mutex::new(HashMap::new()),
            connections: None,
        }
    }

    /// Replace the clock (tests; hosts with their own).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The host's store for each caller's provider grants (`installOAuthConnectionStore`). Without one,
    /// `connection` / `disconnect` report `NOT_CONFIGURED` as TS does on a host with none installed.
    pub fn with_connection_store(mut self, store: Arc<dyn OAuthConnectionStore>) -> Self {
        self.connections = Some(store);
        self
    }

    fn connection_store(&self) -> Result<&Arc<dyn OAuthConnectionStore>, IntegrationError> {
        self.connections.as_ref().ok_or_else(|| {
            IntegrationError::coded(
                NOT_CONFIGURED,
                "No OAuth connection store is installed on this host",
            )
        })
    }

    /// `completeAuthorization`: consume the pending grant once and exchange the code.
    fn complete_authorization(&self, input: &TokenInput) -> Result<Value, IntegrationError> {
        if self.real {
            self.oidc_token(input)
        } else {
            let (access, refresh, sub) = (
                hex(&self.random_bytes(32)),
                hex(&self.random_bytes(32)),
                hex(&self.random_bytes(8)),
            );
            self.mock
                .lock()
                .expect("mock lock")
                .token(input, access, refresh, sub, (self.clock)())
        }
    }

    /// `callerOf`: a provider connection is the signed-in caller's own.
    fn caller_of(ctx: &CallContext) -> Result<&str, IntegrationError> {
        match ctx.principal() {
            Some(principal) if !principal.is_empty() => Ok(principal),
            _ => Err(IntegrationError::coded(
                AUTH_ERROR,
                "A provider connection needs a signed-in caller",
            )),
        }
    }

    /// Replace the random source (tests).
    pub fn with_rng(mut self, rng: Rng) -> Self {
        self.rng = rng;
        self
    }

    fn random_bytes(&self, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        (self.rng)(&mut bytes);
        bytes
    }

    fn client_for(&self, kind: OAuthProviderKind) -> Result<Arc<OidcClient>, IntegrationError> {
        let mut clients = self.clients.lock().expect("clients lock");
        if let Some(cached) = clients.get(&kind) {
            return Ok(cached.clone());
        }
        let server = match kind.static_metadata() {
            Some(metadata) if self.config.issuer_url.is_empty() => OidcServer::Static(metadata),
            _ => OidcServer::Issuer(self.issuer_for(kind)?),
        };
        let client = Arc::new(OidcClient::new(
            kind.name(),
            &self.config.client_id,
            &self.config.client_secret,
            server,
        ));
        clients.insert(kind, client.clone());
        Ok(client)
    }

    fn issuer_for(&self, kind: OAuthProviderKind) -> Result<String, IntegrationError> {
        if !self.config.issuer_url.is_empty() {
            return Ok(self.config.issuer_url.clone());
        }
        kind.issuer().map(str::to_string).ok_or_else(|| {
            IntegrationError::plain(format!(
                "Provider \"{}\" has no OIDC issuer \u{2014} set OIDC_ISSUER_URL to an OIDC-compliant issuer, or use OAUTH_MODE=mock",
                kind.name()
            ))
        })
    }

    fn oidc_authorize(&self, input: &AuthorizeInput) -> Result<Value, IntegrationError> {
        let client = self.client_for(input.provider)?;
        let redirect = if input.redirect_uri.is_empty() {
            self.config.redirect_uri.clone()
        } else {
            input.redirect_uri.clone()
        };
        let extra: Vec<(&str, &str)> = if input.provider == OAuthProviderKind::Google {
            vec![("access_type", "offline"), ("prompt", "consent")]
        } else {
            Vec::new()
        };
        let (auth_url, state, pending) = client.authorize(
            self.transport.as_ref(),
            &redirect,
            &input.scopes,
            input.subject.as_deref(),
            &extra,
            |len| self.random_bytes(len),
        )?;
        let now = (self.clock)();
        let mut grants = self.pending.lock().expect("pending lock");
        grants.insert(state.clone(), (pending, now + PENDING_GRANT_TTL_MS));
        grants.retain(|_, (_, expires_at)| *expires_at >= now);
        Ok(json!({ "authUrl": auth_url, "state": state }))
    }

    /// `takePendingGrant`: the grant under `state`, consumed once, for the subject that started it.
    fn take_pending(
        &self,
        state: &str,
        subject: Option<&str>,
    ) -> Result<Pending, IntegrationError> {
        let entry = self.pending.lock().expect("pending lock").remove(state);
        let grant = match entry {
            Some((grant, expires_at)) if expires_at >= (self.clock)() => grant,
            _ => return Err(grant_not_found(state)),
        };
        if grant.subject.as_deref() != subject {
            return Err(subject_mismatch());
        }
        Ok(grant)
    }

    fn oidc_token(&self, input: &TokenInput) -> Result<Value, IntegrationError> {
        let grant = self.take_pending(&input.state, input.subject.as_deref())?;
        let kind = OAuthProviderKind::parse(&grant.provider)
            .ok_or_else(|| IntegrationError::plain("unknown provider on a pending grant"))?;
        let client = self.client_for(kind)?;
        let tokens = client.exchange(
            self.transport.as_ref(),
            &grant,
            &input.code,
            &input.state,
            (self.clock)(),
        )?;
        self.remember_subject(&tokens);
        let mut out = Map::new();
        out.insert("accessToken".into(), json!(tokens.access_token));
        out.insert(
            "refreshToken".into(),
            json!(tokens.refresh_token.clone().unwrap_or_default()),
        );
        out.insert("expiresIn".into(), expires_in(&tokens));
        out.insert("tokenType".into(), json!("bearer"));
        out.insert("scope".into(), json!(tokens.scope));
        if let Some(subject) = &grant.subject {
            out.insert("subject".into(), json!(subject));
        }
        Ok(Value::Object(out))
    }

    fn remember_subject(&self, tokens: &Tokens) {
        if let Some(sub) = &tokens.id_token_subject {
            self.subjects
                .lock()
                .expect("subjects lock")
                .insert(tokens.access_token.clone(), sub.clone());
        }
    }

    fn oidc_refresh(&self, refresh_token: &str) -> Result<Value, IntegrationError> {
        let client = self.client_for(OAuthProviderKind::Google)?;
        let tokens = client.refresh(self.transport.as_ref(), refresh_token, (self.clock)())?;
        self.remember_subject(&tokens);
        Ok(json!({ "accessToken": tokens.access_token, "expiresIn": expires_in(&tokens) }))
    }

    fn oidc_revoke(&self, token: &str) -> Result<Value, IntegrationError> {
        let client = self.client_for(OAuthProviderKind::Google)?;
        client.revoke(self.transport.as_ref(), token)?;
        self.subjects.lock().expect("subjects lock").remove(token);
        Ok(json!({ "revoked": true }))
    }

    fn oidc_userinfo(&self, access_token: &str) -> Result<Value, IntegrationError> {
        let client = self.client_for(OAuthProviderKind::Google)?;
        let expected = self
            .subjects
            .lock()
            .expect("subjects lock")
            .get(access_token)
            .cloned();
        let info = client.userinfo(
            self.transport.as_ref(),
            access_token,
            expected.as_deref(),
            (self.clock)(),
        )?;
        let field = |key: &str| {
            info.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        Ok(json!({
            "sub": field("sub"),
            "email": field("email"),
            "name": field("name"),
            "picture": field("picture"),
        }))
    }
}

fn expires_in(tokens: &Tokens) -> Value {
    crate::json::number(tokens.expires_in.unwrap_or(3600.0))
}

pub(crate) struct AuthorizeInput {
    pub provider: OAuthProviderKind,
    pub scopes: Vec<String>,
    pub redirect_uri: String,
    pub subject: Option<String>,
}

pub(crate) struct TokenInput {
    pub code: String,
    pub state: String,
    pub subject: Option<String>,
}

/// `OAuthGrantNotFoundError`.
pub(crate) fn grant_not_found(state: &str) -> IntegrationError {
    IntegrationError::plain(format!("Invalid or expired state token: {state}"))
}

/// `OAuthSubjectMismatchError`.
pub(crate) fn subject_mismatch() -> IntegrationError {
    IntegrationError::plain(
        "OAuth completion refused: the completing subject is not the one that started the authorization",
    )
}

impl Provider for OauthProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "authorize" => {
                let input = zod::authorize_input(params)?;
                if self.real {
                    self.oidc_authorize(&input)
                } else {
                    let state = hex(&self.random_bytes(16));
                    Ok(self
                        .mock
                        .lock()
                        .expect("mock lock")
                        .authorize(&input, state))
                }
            }
            "token" => self.complete_authorization(&zod::token_input(params)?),
            "refresh" => {
                let refresh_token = zod::string_field(params, "refreshToken")?;
                if self.real {
                    self.oidc_refresh(&refresh_token)
                } else {
                    let access = hex(&self.random_bytes(32));
                    self.mock.lock().expect("mock lock").refresh(
                        &refresh_token,
                        access,
                        (self.clock)(),
                    )
                }
            }
            "revoke" => {
                let token = zod::string_field(params, "token")?;
                if self.real {
                    self.oidc_revoke(&token)
                } else {
                    Ok(self.mock.lock().expect("mock lock").revoke(&token))
                }
            }
            "userinfo" => {
                let access_token = zod::string_field(params, "accessToken")?;
                if self.real {
                    self.oidc_userinfo(&access_token)
                } else {
                    self.mock
                        .lock()
                        .expect("mock lock")
                        .userinfo(&access_token, (self.clock)())
                }
            }
            "connect" => {
                let (provider, code, state) = zod::connect_input(params)?;
                let principal = Self::caller_of(ctx)?;
                let store = self.connection_store()?;
                let grant = self.complete_authorization(&TokenInput {
                    code,
                    state,
                    subject: Some(principal.to_string()),
                })?;
                let Value::Object(grant) = grant else {
                    return Err(IntegrationError::plain(
                        "The token exchange did not return a grant object",
                    ));
                };
                store.save(principal, provider, &grant).map(|r| r.to_json())
            }
            "connection" | "disconnect" => {
                let provider = zod::provider_input(params)?;
                let store = self.connection_store()?;
                let principal = Self::caller_of(ctx)?;
                let row = if action == "connection" {
                    store.status(principal, provider)
                } else {
                    store.remove(principal, provider)
                };
                row.map(|r| r.to_json())
            }
            other => Err(IntegrationError::plain(format!(
                "Unknown oauth action: {other}"
            ))),
        }
    }
}

/// Lowercase hex of `bytes`: the twin of `generateToken`'s alphabet.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
