//! The `push` service: the Rust twin of `PushIntegration` (VAPID-signed Web Push over the `web-push` SDK).
//! Action: `send`. Reproduced from the SDK: the RFC 8291 `aes128gcm` payload encryption (`http_ece`), the VAPID
//! ES256 JWT, the request headers, and every validation message.

mod crypto;

use std::sync::Arc;

use serde_json::{json, Map, Value};

use self::crypto::{encrypt_aes128gcm, vapid_jwt, EncryptError};
pub use self::crypto::{Entropy, SystemEntropy};
use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport, TransportError};
use crate::wallclock::{system_clock, Clock};

pub const SERVICE: &str = "push";

const TTL_SECONDS: u32 = 2_419_200;
const VAPID_EXPIRATION_SECONDS: i64 = 12 * 60 * 60;

pub struct PushProvider {
    transport: Arc<dyn Transport>,
    vapid_public_key: String,
    vapid_private_key: String,
    vapid_subject: String,
    entropy: Arc<dyn Entropy>,
    clock: Clock,
}

/// Configured when `VAPID_PUBLIC_KEY`, `VAPID_PRIVATE_KEY` and `VAPID_SUBJECT` are all present, as the manager does.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    let all = ["VAPID_PUBLIC_KEY", "VAPID_PRIVATE_KEY", "VAPID_SUBJECT"]
        .iter()
        .all(|name| creds.get(name).is_some());
    if all {
        integrations.register(SERVICE, Arc::new(PushProvider::new(creds, transport)));
    } else {
        integrations.register_unconfigured(SERVICE);
    }
}

/// The headers and the encrypted body of one push request.
type RequestDetails = (Vec<(String, String)>, Vec<u8>);

struct Subscription {
    endpoint: String,
    p256dh: String,
    auth: String,
}

impl PushProvider {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        let read = |name: &str| creds.get(name).unwrap_or("").to_string();
        Self {
            transport,
            vapid_public_key: read("VAPID_PUBLIC_KEY"),
            vapid_private_key: read("VAPID_PRIVATE_KEY"),
            vapid_subject: read("VAPID_SUBJECT"),
            entropy: Arc::new(SystemEntropy),
            clock: system_clock(),
        }
    }

    /// Replace the source of the ephemeral key and salt (tests pin them to reproduce a recorded ciphertext).
    pub fn with_entropy(mut self, entropy: Arc<dyn Entropy>) -> Self {
        self.entropy = entropy;
        self
    }

    /// Replace the clock the VAPID expiry is measured from.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn send(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let subscription = to_subscription(params.get("subscription"))?;
        let payload = payload_json(params);
        let (headers, ciphertext) = self.request_details(&subscription, &payload)?;
        let response = self
            .transport
            .send(&HttpRequest {
                method: "POST".to_string(),
                url: subscription.endpoint.clone(),
                headers,
                body: ciphertext,
                timeout: None,
            })
            .map_err(|failure| match failure {
                TransportError::TimedOut => IntegrationError::plain("Socket timeout"),
                TransportError::Failed(detail) => IntegrationError::plain(detail),
            })?;
        if (200..=299).contains(&response.status) {
            return Ok(json!({ "statusCode": response.status, "ok": true, "expired": false }));
        }
        if response.status >= 500 {
            return Err(IntegrationError::plain(format!(
                "Push endpoint returned {}",
                response.status
            )));
        }
        Ok(json!({
            "statusCode": response.status,
            "ok": false,
            "expired": response.status == 404 || response.status == 410,
        }))
    }

    /// `webpush.generateRequestDetails(subscription, payload, { vapidDetails })`.
    fn request_details(
        &self,
        subscription: &Subscription,
        payload: &str,
    ) -> Result<RequestDetails, IntegrationError> {
        if subscription.endpoint.is_empty() {
            return Err(IntegrationError::plain(
                "You must pass in a subscription with at least an endpoint.",
            ));
        }
        if subscription.p256dh.is_empty() || subscription.auth.is_empty() {
            return Err(IntegrationError::plain(
                "To send a message with a payload, the subscription must have 'auth' and 'p256dh' keys.",
            ));
        }
        let cipher = encrypt_aes128gcm(
            &subscription.p256dh,
            &subscription.auth,
            payload.as_bytes(),
            self.entropy.as_ref(),
        )
        .map_err(|EncryptError(message)| IntegrationError::plain(message))?;

        let mut headers = vec![
            ("TTL".to_string(), TTL_SECONDS.to_string()),
            ("Content-Length".to_string(), cipher.len().to_string()),
            (
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            ),
            ("Content-Encoding".to_string(), "aes128gcm".to_string()),
        ];
        if !subscription
            .endpoint
            .starts_with("https://android.googleapis.com/gcm/send")
        {
            let audience = legacy_audience(&subscription.endpoint);
            let jwt = vapid_jwt(
                &audience,
                &self.vapid_subject,
                &self.vapid_public_key,
                &self.vapid_private_key,
                (self.clock)().div_euclid(1000) + VAPID_EXPIRATION_SECONDS,
            )
            .map_err(|EncryptError(message)| IntegrationError::plain(message))?;
            headers.push((
                "Authorization".to_string(),
                format!("vapid t={jwt}, k={}", self.vapid_public_key),
            ));
        }
        headers.push(("Urgency".to_string(), "normal".to_string()));
        Ok((headers, cipher))
    }
}

impl Provider for PushProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "send" => self.send(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

fn to_subscription(value: Option<&Value>) -> Result<Subscription, IntegrationError> {
    let fields = |v: &Value| -> Option<Subscription> {
        let object = v.as_object()?;
        let endpoint = object.get("endpoint")?.as_str()?;
        let keys = object.get("keys")?.as_object()?;
        Some(Subscription {
            endpoint: endpoint.to_string(),
            p256dh: keys.get("p256dh")?.as_str()?.to_string(),
            auth: keys.get("auth")?.as_str()?.to_string(),
        })
    };
    value.and_then(fields).ok_or_else(|| {
        IntegrationError::plain(
            "push.send: subscription must be { endpoint, keys: { p256dh, auth } }",
        )
    })
}

/// `JSON.stringify({ title, body, url: url || undefined, icon: icon || undefined })`, keys in that order.
fn payload_json(params: &Map<String, Value>) -> String {
    let string = |name: &str| params.get(name).cloned().unwrap_or(Value::Null).to_string();
    let optional = |name: &str| -> Option<String> {
        params
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| Value::String(s.to_string()).to_string())
    };
    let mut out = format!(
        "{{\"title\":{},\"body\":{}",
        string("title"),
        string("body")
    );
    if let Some(url) = optional("url") {
        out.push_str(&format!(",\"url\":{url}"));
    }
    if let Some(icon) = optional("icon") {
        out.push_str(&format!(",\"icon\":{icon}"));
    }
    out.push('}');
    out
}

/// `protocol + '//' + host` of Node's legacy `url.parse(endpoint)`: the host keeps an explicit port and is lowercased.
fn legacy_audience(endpoint: &str) -> String {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return "null//null".to_string();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        host.to_ascii_lowercase()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audience_is_the_origin_with_an_explicit_port_kept() {
        assert_eq!(
            legacy_audience("https://Push.Example.test/send/a?b=1"),
            "https://push.example.test"
        );
        assert_eq!(
            legacy_audience("https://push.example.test:8443/x"),
            "https://push.example.test:8443"
        );
        assert_eq!(
            legacy_audience("https://user:pw@push.example.test/x"),
            "https://push.example.test"
        );
    }

    #[test]
    fn the_payload_drops_empty_optional_fields_and_escapes_strings() {
        let params: Map<String, Value> =
            serde_json::from_str(r#"{"title":"a\"b","body":"c","url":"","icon":"i"}"#)
                .expect("json");
        assert_eq!(
            payload_json(&params),
            r#"{"title":"a\"b","body":"c","icon":"i"}"#
        );
    }

    #[test]
    fn a_subscription_must_be_the_exact_shape() {
        for bad in [
            json!({}),
            json!({"endpoint": "e"}),
            json!({"endpoint": "e", "keys": {"p256dh": "a"}}),
            json!("s"),
            json!([1]),
        ] {
            assert!(to_subscription(Some(&bad)).is_err(), "{bad}");
        }
        assert!(to_subscription(Some(
            &json!({"endpoint": "e", "keys": {"p256dh": "a", "auth": "b"}})
        ))
        .is_ok());
    }
}
