//! The OIDC backend: the Rust twin of `OidcClient` (`@almadar/auth/server`) over `openid-client` 6.8.7 and
//! `oauth4webapi` 3.8.7. Authorization code + PKCE, discovery, refresh, revocation and userinfo. Failures carry the
//! coarse text `openid-client` raises (`unexpected HTTP response status code`, ...), never a code.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::IntegrationError;
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};
use crate::uricomponent::form_urlencode;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const FORM_TYPE: &str = "application/x-www-form-urlencoded;charset=UTF-8";
const CLOCK_TOLERANCE_S: f64 = 30.0;

pub(super) enum OidcServer {
    Issuer(String),
    Static(Map<String, Value>),
}

/// Server-held state of one authorization between `authorize` and the code exchange.
pub(super) struct Pending {
    pub provider: String,
    pub redirect_uri: String,
    pub verifier: String,
    pub subject: Option<String>,
}

pub(super) struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<f64>,
    pub scope: Vec<String>,
    pub id_token_subject: Option<String>,
}

pub(super) struct OidcClient {
    provider: String,
    client_id: String,
    client_secret: String,
    server: OidcServer,
    metadata: Mutex<Option<Arc<Map<String, Value>>>>,
}

type Failure = IntegrationError;

fn fail(message: &str) -> Failure {
    IntegrationError::plain(message)
}

fn invalid_response() -> Failure {
    fail("invalid response encountered")
}
fn parse_error() -> Failure {
    fail("parsing error occured")
}
fn timestamp_error() -> Failure {
    fail("JWT timestamp claim value failed validation")
}
fn claim_error() -> Failure {
    fail("unexpected JWT claim value encountered")
}
fn status_error() -> Failure {
    fail("unexpected HTTP response status code")
}
fn content_type_error() -> Failure {
    fail("unexpected response content-type")
}

fn send(transport: &dyn Transport, request: &HttpRequest) -> Result<HttpResponse, Failure> {
    transport.send(request).map_err(|e| match e {
        TransportError::TimedOut => fail("operation timed out"),
        TransportError::Failed(_) => fail("fetch failed"),
    })
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_urlencode(k), form_urlencode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `Response.headers.get('content-type')?.split(';')[0]`.
fn content_type(response: &HttpResponse) -> Option<String> {
    response
        .header("content-type")
        .map(|v| v.split(';').next().unwrap_or_default().to_string())
}

/// `assertApplicationJson` + `response.json()`: a top-level object, or the failure `openid-client` reports.
fn json_body(response: &HttpResponse) -> Result<Map<String, Value>, Failure> {
    match serde_json::from_str::<Value>(&response.text()) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(invalid_response()),
        Err(_) => {
            if content_type(response).as_deref() == Some("application/json") {
                Err(parse_error())
            } else {
                Err(content_type_error())
            }
        }
    }
}

/// Whether a `WWW-Authenticate` header parses as a challenge (scheme token, then nothing or whitespace/comma).
pub(super) fn has_challenge(response: &HttpResponse) -> bool {
    let Some(header) = response.header("www-authenticate") else {
        return false;
    };
    let is_token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    let scheme_len = header.chars().take_while(|c| is_token(*c)).count();
    scheme_len > 0
        && header[scheme_len..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == ',')
}

fn string_of<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

/// `assertString`: a non-empty string, else an invalid response.
fn non_empty_string(map: &Map<String, Value>, key: &str) -> Result<String, Failure> {
    match map.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(invalid_response()),
    }
}

/// JS `parseFloat` on a string: the longest numeric prefix, else NaN.
pub(super) fn js_parse_float(text: &str) -> f64 {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"^\s*([+-]?(?:Infinity|\d+\.?\d*(?:[eE][+-]?\d+)?|\.\d+(?:[eE][+-]?\d+)?))",
        )
        .expect("static pattern")
    });
    match pattern.captures(text) {
        Some(caps) => {
            let matched = &caps[1];
            if matched.ends_with("Infinity") {
                if matched.starts_with('-') {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            } else {
                matched.parse().unwrap_or(f64::NAN)
            }
        }
        None => f64::NAN,
    }
}

/// `resolveEndpoint`: the named endpoint of the server metadata, which must be an https URL.
fn resolve_endpoint(meta: &Map<String, Value>, name: &str) -> Result<Url, Failure> {
    let url = string_of(meta, name)
        .and_then(|text| Url::parse(text).ok())
        .ok_or_else(|| {
            IntegrationError::plain(format!(
                "authorization server metadata does not contain a valid \"as.{name}\""
            ))
        })?;
    check_https(&url)?;
    Ok(url)
}

fn check_https(url: &Url) -> Result<(), Failure> {
    if url.scheme() != "https" {
        return Err(fail("only requests to HTTPS are allowed"));
    }
    Ok(())
}

fn b64u_text(part: &str) -> Result<String, Failure> {
    let bytes = URL_SAFE_NO_PAD
        .decode(part.trim_end_matches('='))
        .map_err(|_| parse_error())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

enum AlgRule<'a> {
    /// ID tokens: the issuer's advertised algorithms, else RS256.
    IdToken(Option<&'a Vec<Value>>),
    /// A signed UserInfo response: the issuer's advertised algorithms, or no way to check.
    UserInfo(Option<&'a Vec<Value>>),
}

struct Jwt {
    claims: Map<String, Value>,
}

/// `validateJwt`: structure, algorithm and timestamps. The signature is not checked, as `oauth4webapi` does not
/// for responses received straight from the issuer over TLS.
fn validate_jwt(jws: &str, alg: &AlgRule, now_s: f64) -> Result<Jwt, Failure> {
    let parts: Vec<&str> = jws.split('.').collect();
    if parts.len() == 5 {
        return Err(fail("unsupported operation"));
    }
    if parts.len() != 3 {
        return Err(invalid_response());
    }
    let header = match serde_json::from_str::<Value>(&b64u_text(parts[0])?) {
        Ok(Value::Object(map)) => map,
        Ok(_) => return Err(invalid_response()),
        Err(_) => return Err(parse_error()),
    };
    let header_alg = header.get("alg").cloned().unwrap_or(Value::Null);
    match alg {
        AlgRule::IdToken(Some(list)) | AlgRule::UserInfo(Some(list)) => {
            if !list.contains(&header_alg) {
                return Err(invalid_response());
            }
        }
        AlgRule::IdToken(None) => {
            if header_alg != Value::String("RS256".to_string()) {
                return Err(invalid_response());
            }
        }
        AlgRule::UserInfo(None) => return Err(IntegrationError::plain(
            "missing client or server configuration to verify used JWT \"alg\" header parameter",
        )),
    }
    if header.contains_key("crit") {
        return Err(fail("unsupported operation"));
    }
    let claims = match serde_json::from_str::<Value>(&b64u_text(parts[1])?) {
        Ok(Value::Object(map)) => map,
        Ok(_) => return Err(invalid_response()),
        Err(_) => return Err(parse_error()),
    };
    if let Some(exp) = claims.get("exp") {
        let exp = exp.as_f64().ok_or_else(invalid_response)?;
        if exp <= now_s - CLOCK_TOLERANCE_S {
            return Err(timestamp_error());
        }
    }
    if let Some(iat) = claims.get("iat") {
        iat.as_f64().ok_or_else(invalid_response)?;
    }
    if let Some(iss) = claims.get("iss") {
        if !iss.is_string() {
            return Err(invalid_response());
        }
    }
    if let Some(nbf) = claims.get("nbf") {
        let nbf = nbf.as_f64().ok_or_else(invalid_response)?;
        if nbf > now_s + CLOCK_TOLERANCE_S {
            return Err(timestamp_error());
        }
    }
    if let Some(aud) = claims.get("aud") {
        if !aud.is_string() && !aud.is_array() {
            return Err(invalid_response());
        }
    }
    Ok(Jwt { claims })
}

fn validate_issuer(meta: &Map<String, Value>, claims: &Map<String, Value>) -> Result<(), Failure> {
    if claims.get("iss") != meta.get("issuer") || claims.get("iss").is_none() {
        return Err(claim_error());
    }
    Ok(())
}

fn validate_audience(client_id: &str, claims: &Map<String, Value>) -> Result<(), Failure> {
    let ok = match claims.get("aud") {
        Some(Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(client_id)),
        Some(Value::String(aud)) => aud == client_id,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(claim_error())
    }
}

/// The URL of the discovery document: `<issuer path>/.well-known/openid-configuration`, a doubled slash collapsed.
pub(super) fn discovery_url(issuer: &Url) -> Url {
    let mut url = issuer.clone();
    let path = format!("{}/.well-known/openid-configuration", url.path());
    url.set_path(&path.replacen("//", "/", 1));
    url
}

impl OidcClient {
    pub(super) fn new(
        provider: &str,
        client_id: &str,
        client_secret: &str,
        server: OidcServer,
    ) -> Self {
        Self {
            provider: provider.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            server,
            metadata: Mutex::new(None),
        }
    }

    /// The server metadata, discovered once; a failed discovery is retried by the next call.
    fn resolve(&self, transport: &dyn Transport) -> Result<Arc<Map<String, Value>>, Failure> {
        if let Some(cached) = self.metadata.lock().expect("metadata lock").clone() {
            return Ok(cached);
        }
        let meta = Arc::new(match &self.server {
            OidcServer::Static(metadata) => metadata.clone(),
            OidcServer::Issuer(issuer) => self.discover(transport, issuer)?,
        });
        *self.metadata.lock().expect("metadata lock") = Some(meta.clone());
        Ok(meta)
    }

    fn discover(
        &self,
        transport: &dyn Transport,
        issuer: &str,
    ) -> Result<Map<String, Value>, Failure> {
        let server = Url::parse(issuer).map_err(|_| fail("Invalid URL"))?;
        check_https(&server)?;
        let request = HttpRequest {
            method: "GET".into(),
            url: discovery_url(&server).to_string(),
            headers: vec![("Accept".into(), "application/json".into())],
            body: Vec::new(),
            timeout: Some(DISCOVERY_TIMEOUT),
        };
        let response = send(transport, &request)?;
        if response.status != 200 {
            return Err(status_error());
        }
        let meta = json_body(&response)?;
        let advertised = non_empty_string(&meta, "issuer")?;
        let advertised = Url::parse(&advertised).map_err(|_| fail("Invalid URL"))?;
        if advertised.as_str() != server.as_str() {
            return Err(fail(
                "discovered metadata issuer does not match the expected issuer",
            ));
        }
        Ok(meta)
    }

    /// Build the authorization URL and the pending grant (PKCE verifier, subject) the caller stores under `state`.
    pub(super) fn authorize(
        &self,
        transport: &dyn Transport,
        redirect_uri: &str,
        scopes: &[String],
        subject: Option<&str>,
        extra: &[(&str, &str)],
        random: impl Fn(usize) -> Vec<u8>,
    ) -> Result<(String, String, Pending), Failure> {
        let meta = self.resolve(transport)?;
        let state = URL_SAFE_NO_PAD.encode(random(32));
        let verifier = URL_SAFE_NO_PAD.encode(random(32));
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut endpoint = resolve_endpoint(&meta, "authorization_endpoint")?;

        let scope = scopes.join(" ");
        let mut params: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        for (k, v) in [
            ("redirect_uri", redirect_uri),
            ("scope", scope.as_str()),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ] {
            params.push((k.to_string(), v.to_string()));
        }
        if !params.iter().any(|(k, _)| k == "client_id") {
            params.push(("client_id".into(), self.client_id.clone()));
        }
        if !params
            .iter()
            .any(|(k, _)| k == "request_uri" || k == "request")
            && !params.iter().any(|(k, _)| k == "response_type")
        {
            params.push(("response_type".into(), "code".into()));
        }
        let mut all: Vec<(String, String)> = endpoint
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        all.extend(params);
        endpoint.set_query(None);
        endpoint.query_pairs_mut().extend_pairs(all);

        let pending = Pending {
            provider: self.provider.clone(),
            redirect_uri: redirect_uri.to_string(),
            verifier,
            subject: subject.map(str::to_string),
        };
        Ok((endpoint.to_string(), state, pending))
    }

    /// Exchange the code of a grant taken from the pending store.
    pub(super) fn exchange(
        &self,
        transport: &dyn Transport,
        grant: &Pending,
        code: &str,
        state: &str,
        now_ms: i64,
    ) -> Result<Tokens, Failure> {
        if grant.provider != self.provider {
            return Err(IntegrationError::plain(format!(
                "grant was issued by {}, not {}",
                grant.provider, self.provider
            )));
        }
        let mut callback = Url::parse(&grant.redirect_uri).map_err(|_| fail("Invalid URL"))?;
        let meta = self.resolve(transport)?;

        let mut query: Vec<(String, String)> = callback
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .filter(|(k, _)| k != "code" && k != "state")
            .collect();
        query.push(("code".into(), code.to_string()));
        query.push(("state".into(), state.to_string()));
        let has = |name: &str| query.iter().any(|(k, _)| k == name);
        if has("response") {
            return Err(invalid_response());
        }
        let iss = query
            .iter()
            .find(|(k, _)| k == "iss")
            .map(|(_, v)| v.clone());
        let iss_required = meta
            .get("authorization_response_iss_parameter_supported")
            .is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)));
        match &iss {
            None if iss_required => return Err(invalid_response()),
            Some(iss) if !iss.is_empty() && Some(iss.as_str()) != string_of(&meta, "issuer") => {
                return Err(invalid_response())
            }
            _ => {}
        }
        if has("error") {
            return Err(fail("authorization response from the server is an error"));
        }

        callback.set_query(None);
        callback.set_fragment(None);
        let endpoint = resolve_endpoint(&meta, "token_endpoint")?;
        let redirect = callback.to_string();
        let mut body = vec![("redirect_uri", redirect.as_str()), ("code", code)];
        body.push(("code_verifier", grant.verifier.as_str()));
        let response = self.token_request(transport, &endpoint, "authorization_code", body)?;
        self.process_tokens(&meta, &response, now_ms, true)
    }

    pub(super) fn refresh(
        &self,
        transport: &dyn Transport,
        refresh_token: &str,
        now_ms: i64,
    ) -> Result<Tokens, Failure> {
        let meta = self.resolve(transport)?;
        if refresh_token.is_empty() {
            return Err(fail("\"refreshToken\" must not be empty"));
        }
        let endpoint = resolve_endpoint(&meta, "token_endpoint")?;
        let response = self.token_request(
            transport,
            &endpoint,
            "refresh_token",
            vec![("refresh_token", refresh_token)],
        )?;
        self.process_tokens(&meta, &response, now_ms, false)
    }

    pub(super) fn revoke(&self, transport: &dyn Transport, token: &str) -> Result<(), Failure> {
        let meta = self.resolve(transport)?;
        if token.is_empty() {
            return Err(fail("\"token\" must not be empty"));
        }
        let endpoint = resolve_endpoint(&meta, "revocation_endpoint")?;
        let body = form_body(&[
            ("token", token),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
        ]);
        let request = HttpRequest {
            method: "POST".into(),
            url: endpoint.to_string(),
            headers: vec![
                ("Accept".into(), "*/*".into()),
                ("Content-Type".into(), FORM_TYPE.into()),
            ],
            body: body.into_bytes(),
            timeout: None,
        };
        let response = send(transport, &request)?;
        check_body_error(&response, 200)
    }

    /// The UserInfo claims; `expected_subject` is the ID token's `sub` when known.
    pub(super) fn userinfo(
        &self,
        transport: &dyn Transport,
        access_token: &str,
        expected_subject: Option<&str>,
        now_ms: i64,
    ) -> Result<Map<String, Value>, Failure> {
        let meta = self.resolve(transport)?;
        if access_token.is_empty() {
            return Err(fail("\"accessToken\" must not be empty"));
        }
        let endpoint = resolve_endpoint(&meta, "userinfo_endpoint")?;
        let request = HttpRequest {
            method: "GET".into(),
            url: endpoint.to_string(),
            headers: vec![
                ("Accept".into(), "application/json, application/jwt".into()),
                ("Authorization".into(), format!("Bearer {access_token}")),
            ],
            body: Vec::new(),
            timeout: None,
        };
        let response = send(transport, &request)?;
        if has_challenge(&response) {
            return Err(fail(
                "server responded with a challenge in the WWW-Authenticate HTTP Header",
            ));
        }
        if response.status != 200 {
            return Err(status_error());
        }
        let info = if content_type(&response).as_deref() == Some("application/jwt") {
            let algs = meta
                .get("userinfo_signing_alg_values_supported")
                .and_then(Value::as_array);
            let jwt = validate_jwt(
                &response.text(),
                &AlgRule::UserInfo(algs),
                (now_ms / 1000) as f64,
            )?;
            if jwt.claims.contains_key("aud") {
                validate_audience(&self.client_id, &jwt.claims)?;
            }
            if jwt.claims.contains_key("iss") {
                validate_issuer(&meta, &jwt.claims)?;
            }
            jwt.claims
        } else {
            json_body(&response)?
        };
        let sub = non_empty_string(&info, "sub")?;
        if let Some(expected) = expected_subject {
            if sub != expected {
                return Err(fail("unexpected JSON attribute value encountered"));
            }
        }
        Ok(info)
    }

    /// A token-endpoint POST with client authentication in the body (`client_secret_post`).
    fn token_request<'a>(
        &'a self,
        transport: &dyn Transport,
        endpoint: &Url,
        grant_type: &'a str,
        mut pairs: Vec<(&'a str, &'a str)>,
    ) -> Result<HttpResponse, Failure> {
        pairs.push(("grant_type", grant_type));
        pairs.push(("client_id", &self.client_id));
        pairs.push(("client_secret", &self.client_secret));
        let request = HttpRequest {
            method: "POST".into(),
            url: endpoint.to_string(),
            headers: vec![
                ("Accept".into(), "application/json".into()),
                ("Content-Type".into(), FORM_TYPE.into()),
            ],
            body: form_body(&pairs).into_bytes(),
            timeout: None,
        };
        send(transport, &request)
    }

    /// `processGenericAccessTokenResponse` + `tokensOf`.
    fn process_tokens(
        &self,
        meta: &Map<String, Value>,
        response: &HttpResponse,
        now_ms: i64,
        code_grant: bool,
    ) -> Result<Tokens, Failure> {
        check_body_error(response, 200)?;
        let json = json_body(response)?;
        let access_token = non_empty_string(&json, "access_token")?;
        let token_type = non_empty_string(&json, "token_type")?.to_lowercase();

        let expires_in = match json.get("expires_in") {
            None => None,
            Some(value) => {
                let parsed = match value {
                    Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
                    Value::String(s) => js_parse_float(s),
                    Value::Null => f64::NAN,
                    other => js_parse_float(&other.to_string()),
                };
                if !parsed.is_finite() || parsed < 0.0 {
                    return Err(invalid_response());
                }
                Some(parsed)
            }
        };
        let refresh_token = match json.get("refresh_token") {
            None => None,
            Some(_) => Some(non_empty_string(&json, "refresh_token")?),
        };
        let scope = match json.get("scope") {
            None => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(invalid_response()),
        };

        let mut id_token_subject = None;
        if json.contains_key("id_token") {
            let id_token = non_empty_string(&json, "id_token")?;
            let algs = meta
                .get("id_token_signing_alg_values_supported")
                .and_then(Value::as_array);
            let jwt = validate_jwt(&id_token, &AlgRule::IdToken(algs), (now_ms / 1000) as f64)?;
            for claim in ["aud", "exp", "iat", "iss", "sub"] {
                if !jwt.claims.contains_key(claim) {
                    return Err(invalid_response());
                }
            }
            validate_issuer(meta, &jwt.claims)?;
            validate_audience(&self.client_id, &jwt.claims)?;
            if let Some(Value::Array(aud)) = jwt.claims.get("aud") {
                if aud.len() != 1 {
                    match jwt.claims.get("azp") {
                        None => return Err(claim_error()),
                        Some(azp) if azp.as_str() != Some(self.client_id.as_str()) => {
                            return Err(claim_error())
                        }
                        Some(_) => {}
                    }
                }
            }
            if let Some(auth_time) = jwt.claims.get("auth_time") {
                if !auth_time.as_f64().is_some_and(f64::is_finite) {
                    return Err(invalid_response());
                }
            }
            if code_grant && jwt.claims.contains_key("nonce") {
                return Err(claim_error());
            }
            id_token_subject = jwt
                .claims
                .get("sub")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if token_type != "bearer" && token_type != "dpop" {
            return Err(fail("unsupported operation"));
        }

        Ok(Tokens {
            access_token,
            refresh_token,
            expires_in,
            scope: scope
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            id_token_subject,
        })
    }
}

/// `checkOAuthBodyError`: a status other than `expected` is a challenge, an OAuth error body, or just wrong.
fn check_body_error(response: &HttpResponse, expected: u16) -> Result<(), Failure> {
    if response.status == expected {
        return Ok(());
    }
    if has_challenge(response) {
        return Err(fail(
            "server responded with a challenge in the WWW-Authenticate HTTP Header",
        ));
    }
    if (400..500).contains(&response.status) {
        if content_type(response).as_deref() != Some("application/json") {
            return Err(content_type_error());
        }
        if let Ok(Value::Object(body)) = serde_json::from_str::<Value>(&response.text()) {
            if matches!(body.get("error"), Some(Value::String(e)) if !e.is_empty()) {
                return Err(fail("server responded with an error in the response body"));
            }
        }
    }
    Err(status_error())
}
