use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::auth::TokenSource;
use crate::error::IntegrationError;
use crate::transport::{HttpRequest, Transport, TransportError};

pub type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

/// gaxios defaults: three retries on a retryable status, two when no response arrived.
const RETRIES: usize = 3;
const NO_RESPONSE_RETRIES: usize = 2;
const DEFAULT_RETRY_METHODS: [&str; 5] = ["GET", "HEAD", "PUT", "OPTIONS", "DELETE"];

/// The reply to a request that succeeded (2xx).
#[derive(Debug, Clone)]
pub struct ApiResponse {
    pub status: u16,
    /// The body as received.
    pub body: Vec<u8>,
    /// The body as gaxios hands it back: parsed JSON, else the text.
    pub data: Value,
}

pub enum Body {
    None,
    Json(Value),
    Raw { content_type: String, body: Vec<u8> },
}

/// A failed request: the message `DefaultTransporter.processError` settles on, and the response body it read.
#[derive(Debug, Clone)]
pub struct GaxError {
    pub message: String,
    pub body: Option<Value>,
    pub body_text: Option<String>,
}

impl GaxError {
    pub fn into_error(self) -> IntegrationError {
        IntegrationError::plain(self.message)
    }
}

/// gaxios: send, and retry per its rules.
pub struct Gax {
    pub transport: Arc<dyn Transport>,
    pub sleeper: Sleeper,
}

impl Gax {
    /// `retry_methods`: the HTTP methods gaxios retries (`None` is its default list).
    pub fn send(
        &self,
        request: &HttpRequest,
        retry_methods: Option<&[&str]>,
    ) -> Result<ApiResponse, GaxError> {
        let methods = retry_methods.unwrap_or(&DEFAULT_RETRY_METHODS);
        let retryable_method = methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(&request.method));
        let mut attempt = 0usize;
        loop {
            match self.transport.send(request) {
                Err(error) => {
                    if retryable_method && attempt < NO_RESPONSE_RETRIES && attempt < RETRIES {
                        (self.sleeper)(retry_delay(attempt));
                        attempt += 1;
                        continue;
                    }
                    return Err(GaxError {
                        message: match error {
                            TransportError::TimedOut => "network timeout".to_string(),
                            TransportError::Failed(detail) => detail,
                        },
                        body: None,
                        body_text: None,
                    });
                }
                Ok(response) => {
                    let text = response.text();
                    let data = parse_data(&text);
                    if (200..300).contains(&response.status) {
                        return Ok(ApiResponse {
                            status: response.status,
                            body: response.body,
                            data,
                        });
                    }
                    if retryable_method && attempt < RETRIES && status_is_retryable(response.status)
                    {
                        (self.sleeper)(retry_delay(attempt));
                        attempt += 1;
                        continue;
                    }
                    return Err(GaxError {
                        message: error_message(response.status, &data),
                        body: Some(data),
                        body_text: Some(text.into_owned()),
                    });
                }
            }
        }
    }
}

fn status_is_retryable(status: u16) -> bool {
    matches!(status, 100..=199 | 408 | 429 | 500..=599)
}

/// gaxios `getNextRetryDelay`: 100 ms before the first retry, then `(2^attempt - 1) / 2` seconds.
fn retry_delay(attempt: usize) -> Duration {
    if attempt == 0 {
        Duration::from_millis(100)
    } else {
        Duration::from_millis((((1u64 << attempt) - 1) * 1000) / 2)
    }
}

/// gaxios `responseType: 'unknown'`: JSON when the text parses, else the text.
fn parse_data(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
}

pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

pub(super) fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_string(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
        other => other.to_string(),
    }
}

/// `DefaultTransporter.processError`: the message a failed response carries.
pub fn error_message(status: u16, data: &Value) -> String {
    let error = data.get("error").filter(|e| truthy(e));
    if let Some(error) = error {
        return match error {
            Value::String(s) => s.clone(),
            other => match other.get("errors") {
                Some(Value::Array(errors)) => errors
                    .iter()
                    .map(|e| match e.get("message") {
                        Some(m) if !m.is_null() => js_string(m),
                        _ => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => match other.get("message") {
                    Some(m) if !m.is_null() => js_string(m),
                    _ => String::new(),
                },
            },
        };
    }
    if status >= 400 {
        return js_string(data);
    }
    format!("Request failed with status code {status}")
}

/// An authorized client: a token source plus the retrying sender.
pub struct GoogleClient {
    source: Arc<dyn TokenSource>,
    gax: Gax,
}

impl GoogleClient {
    pub fn new(
        source: Arc<dyn TokenSource>,
        transport: Arc<dyn Transport>,
        sleeper: Sleeper,
    ) -> Self {
        Self {
            source,
            gax: Gax { transport, sleeper },
        }
    }

    /// One API call: mint (or reuse) the access token, then send `method url` with the body.
    pub fn request(
        &self,
        method: &str,
        url: &str,
        body: Body,
    ) -> Result<ApiResponse, IntegrationError> {
        let token = self.source.access_token(&self.gax)?;
        let mut headers = vec![
            ("Accept".to_string(), "*/*".to_string()),
            (
                "Authorization".to_string(),
                format!("{} {token}", super::BEARER),
            ),
        ];
        let body = match body {
            Body::None => Vec::new(),
            Body::Json(value) => {
                headers.push(("Content-Type".to_string(), "application/json".to_string()));
                value.to_string().into_bytes()
            }
            Body::Raw { content_type, body } => {
                headers.push(("Content-Type".to_string(), content_type));
                body
            }
        };
        let request = HttpRequest {
            method: method.to_string(),
            url: crate::jswire::fetch_url(url),
            headers,
            body,
            timeout: None,
        };
        self.gax.send(&request, None).map_err(GaxError::into_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backoff_is_100ms_then_half_of_two_to_the_attempt_minus_one() {
        let waits: Vec<u128> = (0..3).map(|a| retry_delay(a).as_millis()).collect();
        assert_eq!(waits, vec![100, 500, 1500]);
    }

    #[test]
    fn error_text_follows_the_transporter() {
        assert_eq!(
            error_message(
                401,
                &json!({ "error": { "message": "Invalid Credentials" } })
            ),
            "Invalid Credentials"
        );
        assert_eq!(
            error_message(
                403,
                &json!({ "error": { "message": "x", "errors": [{ "message": "A" }, {}] } })
            ),
            "A\n"
        );
        assert_eq!(
            error_message(400, &json!({ "error": "bad_request" })),
            "bad_request"
        );
        assert_eq!(
            error_message(400, &json!("upstream says no")),
            "upstream says no"
        );
        assert_eq!(error_message(500, &json!({})), "[object Object]");
        assert_eq!(error_message(400, &json!("")), "");
    }
}
