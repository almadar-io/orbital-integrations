//! The `twilio` service: the Rust twin of `TwilioIntegration` (the Twilio Messages REST resource the
//! `twilio` SDK posts to). Actions: `sendSMS`, `sendWhatsApp`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jsnumber::js_string;
use crate::jswire::base64_encode;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};
use crate::uricomponent::encode_rfc3986;

pub const SERVICE: &str = "twilio";

const API_HOST: &str = "https://api.twilio.com";
const API_VERSION: &str = "2010-04-01";
const REQUEST_TIMEOUT: Duration = Duration::from_millis(30_000);

pub struct TwilioProvider {
    transport: Arc<dyn Transport>,
    account_sid: String,
    auth_token: String,
    phone_number: String,
}

/// Configured when both `TWILIO_ACCOUNT_SID` and `TWILIO_AUTH_TOKEN` are present, as the manager does; a SID the SDK
/// refuses (no `AC` prefix) fails every call with the SDK's own message.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    if creds.get("TWILIO_ACCOUNT_SID").is_none() || creds.get("TWILIO_AUTH_TOKEN").is_none() {
        integrations.register_unconfigured(SERVICE);
        return;
    }
    match TwilioProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl TwilioProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let (Some(account_sid), Some(auth_token)) = (
            creds.get("TWILIO_ACCOUNT_SID"),
            creds.get("TWILIO_AUTH_TOKEN"),
        ) else {
            return Err(IntegrationError::plain(
                "TWILIO_ACCOUNT_SID and TWILIO_AUTH_TOKEN not configured",
            ));
        };
        if !account_sid.starts_with("AC") {
            let api_key = if account_sid.starts_with("SK") {
                ". The given SID indicates an API Key which requires the accountSid to be passed as an additional option"
            } else {
                ""
            };
            return Err(IntegrationError::plain(format!(
                "accountSid must start with AC{api_key}"
            )));
        }
        Ok(Self {
            transport,
            account_sid: account_sid.to_string(),
            auth_token: auth_token.to_string(),
            phone_number: creds.get("TWILIO_PHONE_NUMBER").unwrap_or("").to_string(),
        })
    }

    fn create_message(&self, to: &str, from: &str, body: &str) -> Result<Value, IntegrationError> {
        let form = [("To", to), ("From", from), ("Body", body)]
            .iter()
            .map(|(k, v)| format!("{k}={}", encode_rfc3986(v)))
            .collect::<Vec<_>>()
            .join("&");
        let url = format!(
            "{API_HOST}/{API_VERSION}/Accounts/{}/Messages.json",
            self.account_sid
        );
        let response = self
            .transport
            .send(&HttpRequest {
                method: "POST".to_string(),
                url,
                headers: vec![
                    (
                        "Content-Type".to_string(),
                        "application/x-www-form-urlencoded".to_string(),
                    ),
                    ("Accept".to_string(), "application/json".to_string()),
                    (
                        "Authorization".to_string(),
                        format!(
                            "Basic {}",
                            base64_encode(
                                format!("{}:{}", self.account_sid, self.auth_token).as_bytes()
                            )
                        ),
                    ),
                    (
                        "User-Agent".to_string(),
                        "twilio-node/5.13.1 (orbital-integrations)".to_string(),
                    ),
                ],
                body: form.into_bytes(),
                timeout: Some(REQUEST_TIMEOUT),
            })
            .map_err(|failure| match failure {
                TransportError::TimedOut => IntegrationError::plain(format!(
                    "timeout of {}ms exceeded",
                    REQUEST_TIMEOUT.as_millis()
                )),
                TransportError::Failed(detail) => IntegrationError::plain(detail),
            })?;
        if !(200..300).contains(&response.status) {
            return Err(rest_exception(&response));
        }
        let payload: Value = serde_json::from_str(&response.text())
            .map_err(|e| IntegrationError::plain(e.to_string()))?;
        let mut out = Map::new();
        for (name, field) in [("sid", "sid"), ("status", "status")] {
            if let Some(value) = payload.get(field) {
                out.insert(name.to_string(), value.clone());
            }
        }
        Ok(Value::Object(out))
    }
}

/// `Version.throwException`: the RFC 9457 service exception, else the legacy REST exception.
fn rest_exception(response: &HttpResponse) -> IntegrationError {
    let body: Option<Value> = serde_json::from_str(&response.text()).ok();
    if let Some(Value::Object(fields)) = &body {
        let is_service_exception = fields.get("type").is_some_and(Value::is_string)
            && fields.get("title").is_some_and(Value::is_string)
            && fields.get("status").is_some_and(Value::is_number)
            && fields.get("code").is_some_and(Value::is_number);
        if is_service_exception {
            let status = fields
                .get("status")
                .filter(|s| s.as_f64().is_some_and(|n| n != 0.0))
                .map_or_else(|| response.status.to_string(), js_string);
            let title = fields
                .get("title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .unwrap_or("Failed to execute request");
            return IntegrationError::plain(format!("[HTTP {status}] {title}"));
        }
    }
    match body {
        Some(Value::Null) | None => IntegrationError::plain(format!(
            "[HTTP {}] Failed to execute request",
            response.status
        )),
        Some(other) => IntegrationError::plain(
            other
                .get("message")
                .filter(|m| !m.is_null())
                .map_or_else(String::new, js_string),
        ),
    }
}

fn text(params: &Map<String, Value>, name: &str) -> String {
    params
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

impl Provider for TwilioProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "sendSMS" => {
                let given = text(params, "from");
                let from = if given.is_empty() {
                    self.phone_number.clone()
                } else {
                    given
                };
                self.create_message(&text(params, "to"), &from, &text(params, "body"))
            }
            "sendWhatsApp" => self.create_message(
                &format!("whatsapp:{}", text(params, "to")),
                &format!("whatsapp:{}", self.phone_number),
                &text(params, "body"),
            ),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn creds(pairs: &[(&str, &str)]) -> Credentials {
        Credentials::from_env(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    struct Dead(TransportError);
    impl Transport for Dead {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse, TransportError> {
            Err(self.0.clone())
        }
    }

    #[test]
    fn a_timeout_reads_like_the_sdk_http_client_and_a_dead_connection_keeps_its_detail() {
        let c = creds(&[("TWILIO_ACCOUNT_SID", "ACx"), ("TWILIO_AUTH_TOKEN", "t")]);
        let params = Map::from_iter([
            ("to".to_string(), json!("+1")),
            ("body".to_string(), json!("x")),
        ]);
        let timed_out =
            TwilioProvider::new(&c, Arc::new(Dead(TransportError::TimedOut))).expect("ok");
        assert_eq!(
            timed_out
                .call("sendSMS", &params, &CallContext::anonymous())
                .expect_err("timeout")
                .message,
            "timeout of 30000ms exceeded"
        );
        let dead = TwilioProvider::new(
            &c,
            Arc::new(Dead(TransportError::Failed("connect ECONNREFUSED".into()))),
        )
        .expect("ok");
        assert_eq!(
            dead.call("sendSMS", &params, &CallContext::anonymous())
                .expect_err("down")
                .message,
            "connect ECONNREFUSED"
        );
    }

    #[test]
    fn control_an_ac_sid_is_accepted_and_a_sk_sid_names_the_api_key_hint() {
        let transport: Arc<dyn Transport> = Arc::new(Dead(TransportError::TimedOut));
        assert!(TwilioProvider::new(
            &creds(&[("TWILIO_ACCOUNT_SID", "AC1"), ("TWILIO_AUTH_TOKEN", "t")]),
            transport.clone()
        )
        .is_ok());
        let err = TwilioProvider::new(
            &creds(&[("TWILIO_ACCOUNT_SID", "SK1"), ("TWILIO_AUTH_TOKEN", "t")]),
            transport,
        )
        .err()
        .expect("refused");
        assert!(err.message.contains("API Key"));
    }
}
