//! The `ml` service: the Rust twin of `MLIntegration` (model inference through a deployed serving orbital's
//! generic per-trait event endpoint). Action: `infer`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::{IntegrationError, SERVICE_ERROR};
use crate::jsnumber::{js_number, js_number_from_text, js_string, js_truthy};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport, TransportError};

pub const SERVICE: &str = "ml";

const INFERRED_EVENT: &str = "INFERRED";
const INFER_FAILED_EVENT: &str = "INFER_FAILED";
const DEFAULT_TIMEOUT_MS: f64 = 60_000.0;
const TIMEOUT_ERROR: &str = "TIMEOUT_ERROR";
const NETWORK_ERROR: &str = "NETWORK_ERROR";

pub struct MlProvider {
    transport: Arc<dyn Transport>,
    events_url: String,
    timeout_ms: f64,
}

/// Configured when both `MASAR_URL` and `MASAR_ML_TRAIT` are present, as the manager does.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    match MlProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(_) => integrations.register_unconfigured(SERVICE),
    }
}

impl MlProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let (Some(base_url), Some(trait_prefix)) =
            (creds.get("MASAR_URL"), creds.get("MASAR_ML_TRAIT"))
        else {
            return Err(IntegrationError::plain(
                "ML integration requires MASAR_URL (serving orbital host) and MASAR_ML_TRAIT (kebab-case trait name of its /events route) to be configured",
            ));
        };
        Ok(Self {
            transport,
            events_url: format!(
                "{}/api/{trait_prefix}/events",
                base_url.trim_end_matches('/')
            ),
            timeout_ms: creds
                .get("MASAR_ML_TIMEOUT_MS")
                .map_or(DEFAULT_TIMEOUT_MS, js_number_from_text),
        })
    }

    fn infer(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let model = params.get("model").cloned().unwrap_or(Value::Null);
        let input = params.get("input").cloned().unwrap_or(Value::Null);
        let response = self.post_event(&model, &input)?;

        if !response.get("success").is_some_and(js_truthy) {
            let reason = response
                .get("error")
                .filter(|e| !e.is_null())
                .map_or_else(|| "unknown error".to_string(), js_string);
            return Err(IntegrationError::coded(
                SERVICE_ERROR,
                format!("ML service reported failure: {reason}"),
            ));
        }

        let results: &[Value] = response
            .get("effectResults")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        if let Some(failed) = results
            .iter()
            .find(|r| is_emit(r) && emitted_event(r) == Some(INFER_FAILED_EVENT))
        {
            let reason = failed
                .get("error")
                .filter(|e| !e.is_null())
                .map_or_else(|| "model unavailable".to_string(), js_string);
            return Err(IntegrationError::coded(
                SERVICE_ERROR,
                format!("ML service could not infer: {reason}"),
            ));
        }

        extract_infer_result(results).ok_or_else(|| {
            IntegrationError::coded(
                SERVICE_ERROR,
                "ML service response did not include a valid inference result",
            )
        })
    }

    fn post_event(&self, model: &Value, input: &Value) -> Result<Value, IntegrationError> {
        let body = json!({ "event": "INFER", "payload": { "model": model, "input": input } });
        let timeout = Duration::from_millis(if self.timeout_ms.is_nan() {
            1.0
        } else {
            self.timeout_ms.max(1.0)
        } as u64);
        let response = self
            .transport
            .send(&HttpRequest {
                method: "POST".to_string(),
                url: self.events_url.clone(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("Accept".to_string(), "*/*".to_string()),
                ],
                body: body.to_string().into_bytes(),
                timeout: Some(timeout),
            })
            .map_err(|e| match e {
                TransportError::TimedOut => IntegrationError::coded(
                    TIMEOUT_ERROR,
                    format!(
                        "ML service request timed out after {}ms (model may be cold-starting)",
                        js_number(self.timeout_ms)
                    ),
                ),
                TransportError::Failed(_) => IntegrationError::coded(
                    NETWORK_ERROR,
                    "ML service request failed: fetch failed",
                ),
            })?;
        if !response.is_success() {
            return Err(IntegrationError::coded(
                SERVICE_ERROR,
                format!("ML service responded with status {}", response.status),
            ));
        }
        serde_json::from_str(&response.text()).map_err(|e| {
            IntegrationError::coded(
                SERVICE_ERROR,
                format!("ML service returned a malformed response body: {e}"),
            )
        })
    }
}

fn is_emit(result: &Value) -> bool {
    result.get("effect").and_then(Value::as_str) == Some("emit")
}

fn emitted_event(result: &Value) -> Option<&str> {
    result.get("data")?.get("event")?.as_str()
}

/// The `INFERRED` emit result's `{ output, confidence, violations }`, or `None` on any shape mismatch.
fn extract_infer_result(results: &[Value]) -> Option<Value> {
    let emitted = results.iter().find(|r| {
        is_emit(r)
            && r.get("success").is_some_and(js_truthy)
            && emitted_event(r) == Some(INFERRED_EVENT)
    })?;
    let payload = emitted.get("data")?.get("payload")?.as_object()?;
    let output = payload.get("output")?;
    let confidence = payload.get("confidence")?;
    let violations = payload.get("violations")?;
    (confidence.is_number() && violations.is_array())
        .then(|| json!({ "output": output, "confidence": confidence, "violations": violations }))
}

impl Provider for MlProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "infer" => self.infer(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::HttpResponse;

    struct Fixed(Result<HttpResponse, TransportError>);
    impl Transport for Fixed {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.0.clone()
        }
    }

    fn provider(reply: Result<HttpResponse, TransportError>) -> MlProvider {
        let creds = Credentials::from_env(
            [("MASAR_URL", "http://m"), ("MASAR_ML_TRAIT", "t")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        MlProvider::new(&creds, Arc::new(Fixed(reply))).expect("configured")
    }

    fn infer(p: &MlProvider) -> IntegrationError {
        let params = Map::from_iter([
            ("model".to_string(), json!("m")),
            ("input".to_string(), json!({})),
        ]);
        p.infer(&params).expect_err("fails")
    }

    #[test]
    fn a_timeout_and_a_dead_connection_carry_their_own_codes() {
        let timed_out = infer(&provider(Err(TransportError::TimedOut)));
        assert_eq!(timed_out.code.as_deref(), Some(TIMEOUT_ERROR));
        assert_eq!(
            timed_out.message,
            "ML service request timed out after 60000ms (model may be cold-starting)"
        );
        let dead = infer(&provider(Err(TransportError::Failed("refused".into()))));
        assert_eq!(dead.code.as_deref(), Some(NETWORK_ERROR));
        assert_eq!(dead.message, "ML service request failed: fetch failed");
    }

    #[test]
    fn an_unreadable_body_is_a_service_error() {
        let bad = infer(&provider(Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: "oops".into(),
        })));
        assert_eq!(bad.code.as_deref(), Some(SERVICE_ERROR));
        assert!(bad
            .message
            .starts_with("ML service returned a malformed response body: "));
    }

    #[test]
    fn control_a_valid_reply_infers() {
        let body = json!({"success": true, "effectResults": [{"effect": "emit", "success": true, "data": {"event": "INFERRED", "payload": {"output": 1, "confidence": 0.5, "violations": []}}}]});
        let ok = provider(Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: body.to_string().into_bytes(),
        }));
        let params = Map::from_iter([
            ("model".to_string(), json!("m")),
            ("input".to_string(), json!({})),
        ]);
        assert_eq!(ok.infer(&params).expect("ok")["confidence"], json!(0.5));
    }
}
