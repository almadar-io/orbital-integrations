//! The in-memory backend (`OAUTH_MODE=mock`, or no client credentials): the twin of the mock half of
//! `OAuthIntegration`. Tokens come from the provider's random source.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::{subject_mismatch, AuthorizeInput, OAuthProviderKind, TokenInput};
use crate::error::IntegrationError;
use crate::uricomponent::form_urlencode;

const TOKEN_LIFETIME_MS: i64 = 3600 * 1000;

struct PendingMock {
    provider: OAuthProviderKind,
    scopes: Vec<String>,
    subject: Option<String>,
}

struct TokenSet {
    refresh_token: String,
    expires_at: i64,
}

#[derive(Clone)]
struct UserInfo {
    sub: String,
    email: String,
    name: String,
    picture: String,
}

impl UserInfo {
    fn json(&self) -> Value {
        json!({ "sub": self.sub, "email": self.email, "name": self.name, "picture": self.picture })
    }
}

#[derive(Default)]
pub(super) struct MockBackend {
    states: HashMap<String, PendingMock>,
    tokens: HashMap<String, TokenSet>,
    refresh_index: HashMap<String, String>,
    sessions: HashMap<String, UserInfo>,
}

fn auth_url(provider: OAuthProviderKind) -> &'static str {
    match provider {
        OAuthProviderKind::Google => "https://accounts.google.com/o/oauth2/v2/auth",
        OAuthProviderKind::Github => "https://github.com/login/oauth/authorize",
        OAuthProviderKind::Auth0 => "https://auth.example.com/authorize",
    }
}

fn mock_user(provider: OAuthProviderKind, sub: &str) -> UserInfo {
    let short: String = sub.chars().take(8).collect();
    let name = provider.name();
    UserInfo {
        sub: sub.to_string(),
        email: format!("user-{short}@{name}.example.com"),
        name: format!("Mock User ({name})"),
        picture: format!("https://{name}.example.com/avatar/{short}.png"),
    }
}

impl MockBackend {
    pub(super) fn authorize(&mut self, input: &AuthorizeInput, state: String) -> Value {
        self.states.insert(
            state.clone(),
            PendingMock {
                provider: input.provider,
                scopes: input.scopes.clone(),
                subject: input.subject.clone(),
            },
        );
        let query = [
            ("response_type", "code".to_string()),
            ("scope", input.scopes.join(" ")),
            ("redirect_uri", input.redirect_uri.clone()),
            ("state", state.clone()),
            ("client_id", "mock-client-id".to_string()),
        ]
        .iter()
        .map(|(k, v)| format!("{}={}", form_urlencode(k), form_urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
        json!({ "authUrl": format!("{}?{query}", auth_url(input.provider)), "state": state })
    }

    pub(super) fn token(
        &mut self,
        input: &TokenInput,
        access_token: String,
        refresh_token: String,
        sub: String,
        now: i64,
    ) -> Result<Value, IntegrationError> {
        let pending = self
            .states
            .remove(&input.state)
            .ok_or_else(|| super::grant_not_found(&input.state))?;
        if pending.subject != input.subject {
            return Err(subject_mismatch());
        }
        self.tokens.insert(
            access_token.clone(),
            TokenSet {
                refresh_token: refresh_token.clone(),
                expires_at: now + TOKEN_LIFETIME_MS,
            },
        );
        self.refresh_index
            .insert(refresh_token.clone(), access_token.clone());
        self.sessions
            .insert(access_token.clone(), mock_user(pending.provider, &sub));
        let mut out = json!({
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresIn": 3600,
            "tokenType": "bearer",
            "scope": pending.scopes,
        });
        if let (Some(subject), Value::Object(map)) = (pending.subject, &mut out) {
            map.insert("subject".into(), json!(subject));
        }
        Ok(out)
    }

    pub(super) fn refresh(
        &mut self,
        refresh_token: &str,
        new_access_token: String,
        now: i64,
    ) -> Result<Value, IntegrationError> {
        let old = self
            .refresh_index
            .get(refresh_token)
            .cloned()
            .ok_or_else(|| IntegrationError::plain("Invalid refresh token"))?;
        let user = self.sessions.get(&old).cloned();
        self.tokens.remove(&old);
        self.sessions.remove(&old);
        self.tokens.insert(
            new_access_token.clone(),
            TokenSet {
                refresh_token: refresh_token.to_string(),
                expires_at: now + TOKEN_LIFETIME_MS,
            },
        );
        self.refresh_index
            .insert(refresh_token.to_string(), new_access_token.clone());
        if let Some(user) = user {
            self.sessions.insert(new_access_token.clone(), user);
        }
        Ok(json!({ "accessToken": new_access_token, "expiresIn": 3600 }))
    }

    pub(super) fn revoke(&mut self, token: &str) -> Value {
        if let Some(set) = self.tokens.remove(token) {
            self.refresh_index.remove(&set.refresh_token);
            self.sessions.remove(token);
            return json!({ "revoked": true });
        }
        if let Some(access) = self.refresh_index.remove(token) {
            self.tokens.remove(&access);
            self.sessions.remove(&access);
            return json!({ "revoked": true });
        }
        json!({ "revoked": false })
    }

    pub(super) fn userinfo(
        &mut self,
        access_token: &str,
        now: i64,
    ) -> Result<Value, IntegrationError> {
        let expires_at = self
            .tokens
            .get(access_token)
            .map(|set| set.expires_at)
            .ok_or_else(|| IntegrationError::plain("Invalid access token"))?;
        if now > expires_at {
            self.tokens.remove(access_token);
            self.sessions.remove(access_token);
            return Err(IntegrationError::plain("Access token expired"));
        }
        self.sessions
            .get(access_token)
            .map(UserInfo::json)
            .ok_or_else(|| IntegrationError::plain("No session found for access token"))
    }
}
