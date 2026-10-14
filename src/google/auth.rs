use std::sync::Mutex;

use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde_json::Value;

use super::client::{js_string, truthy, Gax, GaxError};
use crate::error::IntegrationError;
use crate::jswire::{base64_decode_lenient, json_restringify, Clock};
use crate::transport::HttpRequest;

const JWT_TOKEN_URL: &str = "https://www.googleapis.com/oauth2/v4/token";
const REFRESH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// `google-auth-library`'s `eagerRefreshThresholdMillis`: a token this close to expiry is renewed.
const EAGER_REFRESH_MS: u64 = 5 * 60 * 1000;

/// Where an access token comes from. Implementations cache it until it nears expiry.
pub trait TokenSource: Send + Sync {
    fn access_token(&self, gax: &Gax) -> Result<String, IntegrationError>;
}

struct Cached {
    access_token: String,
    /// Epoch ms; `None` when the token endpoint named no lifetime (never renewed).
    expires_at: Option<u64>,
}

impl Cached {
    fn is_expiring(&self, now: u64) -> bool {
        self.expires_at
            .is_some_and(|at| at <= now + EAGER_REFRESH_MS)
    }
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        out.append_pair(k, v);
    }
    out.finish()
}

fn token_reply(gax: &Gax, request: &HttpRequest) -> Result<Value, IntegrationError> {
    let response = gax.send(request, Some(&["POST"])).map_err(reauth_failure)?;
    Ok(response.data)
}

/// A service account (`google.auth.JWT`): signs an RS256 assertion and exchanges it for an access token.
pub struct JwtSource {
    email: Option<String>,
    private_key: Option<String>,
    scope: String,
    subject: Option<String>,
    clock: Clock,
    cached: Mutex<Option<Cached>>,
}

impl JwtSource {
    /// `key_json` is the service-account key as the env carries it: raw JSON, or the JSON base64-encoded.
    pub fn from_key(
        raw: &str,
        scope: &str,
        subject: Option<&str>,
        clock: Clock,
    ) -> Result<Self, IntegrationError> {
        let json_text = if raw.trim_start().starts_with('{') {
            raw.to_string()
        } else {
            String::from_utf8_lossy(&base64_decode_lenient(raw)).into_owned()
        };
        let key: Value = serde_json::from_str(&json_text)
            .map_err(|e| IntegrationError::plain(format!("Unexpected token in JSON: {e}")))?;
        let field = |name: &str| key.get(name).and_then(Value::as_str).map(str::to_string);
        Ok(Self {
            email: field("client_email"),
            private_key: field("private_key"),
            scope: scope.to_string(),
            subject: subject.filter(|s| !s.is_empty()).map(str::to_string),
            clock,
            cached: Mutex::new(None),
        })
    }

    fn assertion(&self, key: &str, email: &str, iat: u64) -> Result<String, IntegrationError> {
        let header = "{\"alg\":\"RS256\"}";
        let quoted = |text: &str| Value::String(text.to_string()).to_string();
        let sub = self
            .subject
            .as_deref()
            .map_or_else(String::new, |sub| format!(",\"sub\":{}", quoted(sub)));
        let payload = format!(
            "{{\"iss\":{},\"scope\":{},\"aud\":{},\"exp\":{},\"iat\":{iat}{sub}}}",
            quoted(email),
            quoted(&self.scope),
            quoted(JWT_TOKEN_URL),
            iat + 3600,
        );
        let signing_input = format!(
            "{}.{}",
            b64url(header.as_bytes()),
            b64url(payload.as_bytes())
        );
        let signature = sign_rs256(key, signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", b64url(&signature)))
    }
}

fn b64url(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    URL_SAFE_NO_PAD.encode(bytes)
}

fn sign_rs256(pem: &str, message: &[u8]) -> Result<Vec<u8>, IntegrationError> {
    let invalid = || IntegrationError::plain("error:1E08010C:DECODER routines::unsupported");
    let label_start = pem.find("-----BEGIN ").ok_or_else(invalid)?;
    let label_end = pem[label_start + 11..].find("-----").ok_or_else(invalid)? + label_start + 11;
    let label = &pem[label_start + 11..label_end];
    let body_end = pem.find("-----END ").ok_or_else(invalid)?;
    let body: String = pem[label_end + 5..body_end].split_whitespace().collect();
    let der = base64_decode_lenient(&body);
    let pair = if label == "RSA PRIVATE KEY" {
        RsaKeyPair::from_der(&der)
    } else {
        RsaKeyPair::from_pkcs8(&der)
    }
    .map_err(|_| invalid())?;
    let mut signature = vec![0u8; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(|_| invalid())?;
    Ok(signature)
}

impl TokenSource for JwtSource {
    fn access_token(&self, gax: &Gax) -> Result<String, IntegrationError> {
        let now = (self.clock)();
        let mut cached = self.cached.lock().expect("token lock");
        if let Some(token) = cached.as_ref() {
            if !token.is_expiring(now) {
                return Ok(token.access_token.clone());
            }
        }
        let Some(key) = self.private_key.as_deref() else {
            return Err(IntegrationError::plain("No key or keyFile set."));
        };
        let Some(email) = self.email.as_deref() else {
            return Err(IntegrationError::plain("email is required."));
        };
        let iat = now / 1000;
        let assertion = self.assertion(key, email, iat)?;
        let request = HttpRequest {
            method: "POST".into(),
            url: JWT_TOKEN_URL.into(),
            headers: vec![
                (
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
                ("Accept".into(), "application/json".into()),
            ],
            body: form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .into_bytes(),
            timeout: None,
        };
        let reply = gax.send(&request, Some(&["POST"])).map_err(token_failure)?;
        let data = reply.data;
        let access_token = text_field(&data, "access_token");
        let expires_at = data
            .get("expires_in")
            .filter(|v| !v.is_null())
            .and_then(Value::as_f64)
            .map(|secs| ((iat as f64 + secs) * 1000.0) as u64);
        *cached = Some(Cached {
            access_token: access_token.clone(),
            expires_at,
        });
        Ok(access_token)
    }
}

/// gtoken rewrites a failed exchange's message to `error: error_description` when the body names one.
fn token_failure(error: GaxError) -> IntegrationError {
    if let Some(body) = error.body.as_ref() {
        if let Some(code) = body.get("error").filter(|e| truthy(e)) {
            let description = body
                .get("error_description")
                .filter(|d| truthy(d))
                .map_or_else(String::new, |d| format!(": {}", js_string(d)));
            return IntegrationError::plain(format!("{}{description}", js_string(code)));
        }
    }
    error.into_error()
}

/// `OAuth2Client.refreshTokenNoCache`: a `ReAuth` invalid_grant reports the whole body.
fn reauth_failure(error: GaxError) -> IntegrationError {
    if error.message == "invalid_grant" {
        let reauth = error
            .body
            .as_ref()
            .and_then(|b| b.get("error_description"))
            .and_then(Value::as_str)
            .is_some_and(|d| d.to_lowercase().contains("reauth"));
        if reauth {
            if let Some(text) = error
                .body_text
                .as_deref()
                .and_then(|t| json_restringify(t).ok())
            {
                return IntegrationError::plain(text);
            }
        }
    }
    error.into_error()
}

fn text_field(data: &Value, name: &str) -> String {
    data.get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// A user's refresh token (`google.auth.OAuth2` with `setCredentials({ refresh_token })`).
pub struct RefreshSource {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    clock: Clock,
    cached: Mutex<Option<Cached>>,
}

impl RefreshSource {
    pub fn new(client_id: &str, client_secret: &str, refresh_token: &str, clock: Clock) -> Self {
        Self {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            refresh_token: refresh_token.to_string(),
            clock,
            cached: Mutex::new(None),
        }
    }
}

impl TokenSource for RefreshSource {
    fn access_token(&self, gax: &Gax) -> Result<String, IntegrationError> {
        let now = (self.clock)();
        let mut cached = self.cached.lock().expect("token lock");
        if let Some(token) = cached.as_ref() {
            if !token.is_expiring(now) {
                return Ok(token.access_token.clone());
            }
        }
        let request = HttpRequest {
            method: "POST".into(),
            url: REFRESH_TOKEN_URL.into(),
            headers: vec![
                (
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                ),
                ("Accept".into(), "*/*".into()),
            ],
            body: form(&[
                ("refresh_token", &self.refresh_token),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("grant_type", "refresh_token"),
            ])
            .into_bytes(),
            timeout: None,
        };
        let reply = token_reply(gax, &request)?;
        let access_token = text_field(&reply, "access_token");
        let expires_at = reply
            .get("expires_in")
            .filter(|v| !v.is_null())
            .and_then(Value::as_f64)
            .map(|secs| now + (secs * 1000.0) as u64);
        *cached = Some(Cached {
            access_token: access_token.clone(),
            expires_at,
        });
        Ok(access_token)
    }
}
