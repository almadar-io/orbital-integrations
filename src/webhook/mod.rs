//! The `webhook` service: the Rust twin of `WebhookIntegration` (signed outbound HTTP POST notifications).
//! Action: `send`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::hmacsha256::hmac_sha256_hex;
use crate::jswire::fetch_failure;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};
use crate::wallclock::{iso8601, system_clock, Clock};

pub const SERVICE: &str = "webhook";

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

pub struct WebhookProvider {
    transport: Arc<dyn Transport>,
    signing_secret: String,
    timeout: Duration,
    clock: Clock,
}

/// `webhook` has no required credentials: the target URL is a per-call param.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    integrations.register(SERVICE, Arc::new(WebhookProvider::new(creds, transport)));
}

impl WebhookProvider {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        let configured = creds
            .get("WEBHOOK_TIMEOUT_MS")
            .map(crate::jsnumber::js_number_from_text)
            .filter(|n| n.is_finite() && *n >= 1.0)
            .map_or(DEFAULT_TIMEOUT_MS, |n| n as u64);
        Self {
            transport,
            signing_secret: creds
                .get("WEBHOOK_SIGNING_SECRET")
                .unwrap_or("")
                .to_string(),
            timeout: Duration::from_millis(configured),
            clock: system_clock(),
        }
    }

    /// Replace the clock that stamps the body and measures the call (tests; hosts with their own clock).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn send(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let url = params.get("url").and_then(Value::as_str).unwrap_or("");
        let event = params.get("event").and_then(Value::as_str).unwrap_or("");
        let payload = match params.get("payload") {
            Some(p) if !p.is_null() => p.clone(),
            _ => json!({}),
        };
        let body = format!(
            "{{\"event\":{},\"payload\":{},\"timestamp\":\"{}\"}}",
            Value::String(event.to_string()),
            payload,
            iso8601((self.clock)())
        );

        let mut headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Almadar-Event".to_string(), event.to_string()),
            ("Accept".to_string(), "*/*".to_string()),
        ];
        let call_secret = params.get("secret").and_then(Value::as_str).unwrap_or("");
        let secret = if call_secret.is_empty() {
            self.signing_secret.as_str()
        } else {
            call_secret
        };
        if !secret.is_empty() {
            headers.push((
                "X-Almadar-Signature".to_string(),
                format!("sha256={}", hmac_sha256_hex(secret, &body)),
            ));
        }

        if url::Url::parse(url).is_err() {
            return Err(IntegrationError::plain("Invalid URL"));
        }
        let started = (self.clock)();
        let response = self
            .transport
            .send(&HttpRequest {
                method: "POST".to_string(),
                url: url.to_string(),
                headers,
                body: body.into_bytes(),
                timeout: Some(self.timeout),
            })
            .map_err(fetch_failure)?;
        if response.status >= 500 {
            return Err(IntegrationError::plain(format!(
                "Webhook endpoint returned {}",
                response.status
            )));
        }
        Ok(json!({
            "status": response.status,
            "ok": response.is_success(),
            "durationMs": (self.clock)() - started,
        }))
    }
}

impl Provider for WebhookProvider {
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
