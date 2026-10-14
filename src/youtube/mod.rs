//! The `youtube` service: the Rust twin of `YouTubeIntegration` (YouTube Data API v3 through `googleapis`).
//! Actions: `search`, `getVideo`, `getChannel`. The SDK's own behavior is reproduced where it shows on the wire or
//! in the result: the repeated-key query string, the retry of 408/429/5xx and dead connections, and the
//! error-message rewrite of `google-auth-library`'s `DefaultTransporter`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jsnumber::{js_number, js_string, js_truthy};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};
use crate::uricomponent::encode_rfc3986;

pub const SERVICE: &str = "youtube";

const API: &str = "https://youtube.googleapis.com/youtube/v3";
const DEFAULT_MAX_RESULTS: f64 = 10.0;
const MAX_RETRIES: usize = 3;
const NO_RESPONSE_RETRIES: usize = 2;

type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

pub struct YouTubeProvider {
    transport: Arc<dyn Transport>,
    api_key: String,
    sleeper: Sleeper,
}

/// Configured when `YOUTUBE_API_KEY` is present, as the manager does.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    match YouTubeProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(_) => integrations.register_unconfigured(SERVICE),
    }
}

impl YouTubeProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let api_key = creds.get("YOUTUBE_API_KEY").unwrap_or("").to_string();
        if api_key.is_empty() {
            return Err(IntegrationError::plain("YOUTUBE_API_KEY not configured"));
        }
        Ok(Self {
            transport,
            api_key,
            sleeper: Arc::new(std::thread::sleep),
        })
    }

    /// Replace the wait between SDK retries (tests; hosts with their own clock).
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    fn url(&self, resource: &str, query: &[(&str, String)]) -> String {
        let pairs: Vec<String> = query
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .chain([("key", self.api_key.as_str())])
            .map(|(k, v)| format!("{k}={}", encode_rfc3986(v)))
            .collect();
        format!("{API}/{resource}?{}", pairs.join("&"))
    }

    /// One Data API GET with the SDK's retry policy: the parsed body (its text when it is not JSON).
    fn get(&self, resource: &str, query: &[(&str, String)]) -> Result<Value, IntegrationError> {
        let url = self.url(resource, query);
        let mut attempt = 0usize;
        loop {
            let outcome = self.transport.send(&HttpRequest {
                method: "GET".to_string(),
                url: url.clone(),
                headers: vec![("Accept".to_string(), "*/*".to_string())],
                body: Vec::new(),
                timeout: None,
            });
            match outcome {
                Ok(response) if accepted(response.status) => return Ok(parse_body(&response)),
                Ok(response) => {
                    if retryable(response.status) && attempt < MAX_RETRIES {
                        (self.sleeper)(retry_delay(attempt));
                        attempt += 1;
                        continue;
                    }
                    return Err(IntegrationError::plain(error_message(&response)));
                }
                Err(failure) => {
                    if attempt < NO_RESPONSE_RETRIES {
                        (self.sleeper)(retry_delay(attempt));
                        attempt += 1;
                        continue;
                    }
                    let reason = match failure {
                        TransportError::TimedOut => {
                            return Err(IntegrationError::plain(format!(
                                "network timeout at: {url}"
                            )))
                        }
                        TransportError::Failed(detail) => detail,
                    };
                    return Err(IntegrationError::plain(format!(
                        "request to {url} failed, reason: {reason}"
                    )));
                }
            }
        }
    }

    fn search(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let query = params.get("query").map_or_else(String::new, js_string);
        let max_results = params
            .get("maxResults")
            .and_then(Value::as_f64)
            .filter(|n| *n != 0.0 && !n.is_nan())
            .unwrap_or(DEFAULT_MAX_RESULTS);
        let mut pairs = vec![
            ("part", "snippet".to_string()),
            ("q", query),
            ("maxResults", js_number(max_results)),
        ];
        if let Some(kind) = params
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            pairs.push(("type", kind.to_string()));
        }
        let data = self.get("search", &pairs)?;
        Ok(match data.get("items").and_then(Value::as_array) {
            Some(items) => Value::Array(
                items
                    .iter()
                    .map(|item| {
                        pick(&[
                            ("videoId", look(item, &["id", "videoId"])),
                            ("title", look(item, &["snippet", "title"])),
                            (
                                "thumbnail",
                                look(item, &["snippet", "thumbnails", "default", "url"]),
                            ),
                            ("description", look(item, &["snippet", "description"])),
                        ])
                    })
                    .collect(),
            ),
            None => Value::Null,
        })
    }

    fn get_video(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = params.get("videoId").map_or_else(String::new, js_string);
        let data = self.get(
            "videos",
            &[
                ("part", "snippet".into()),
                ("part", "statistics".into()),
                ("id", id.clone()),
            ],
        )?;
        let video = first_item(&data)
            .ok_or_else(|| IntegrationError::plain(format!("Video not found: {id}")))?;
        Ok(pick(&[
            ("title", look(video, &["snippet", "title"])),
            ("description", look(video, &["snippet", "description"])),
            ("viewCount", look(video, &["statistics", "viewCount"])),
            ("likeCount", look(video, &["statistics", "likeCount"])),
        ]))
    }

    fn get_channel(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = params.get("channelId").map_or_else(String::new, js_string);
        let data = self.get(
            "channels",
            &[
                ("part", "snippet".into()),
                ("part", "statistics".into()),
                ("id", id.clone()),
            ],
        )?;
        let channel = first_item(&data)
            .ok_or_else(|| IntegrationError::plain(format!("Channel not found: {id}")))?;
        Ok(pick(&[
            ("name", look(channel, &["snippet", "title"])),
            ("description", look(channel, &["snippet", "description"])),
            (
                "subscriberCount",
                look(channel, &["statistics", "subscriberCount"]),
            ),
        ]))
    }
}

impl Provider for YouTubeProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "search" => self.search(params),
            "getVideo" => self.get_video(params),
            "getChannel" => self.get_channel(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

/// `validateStatus` of `googleapis-common`: 2xx and 304.
fn accepted(status: u16) -> bool {
    (200..300).contains(&status) || status == 304
}

/// `gaxios`' `statusCodesToRetry`: 1xx, 408, 429 and 5xx.
fn retryable(status: u16) -> bool {
    matches!(status, 100..=199 | 408 | 429 | 500..=599)
}

/// `gaxios`' backoff: 100 ms, then `(2^attempt - 1) / 2` seconds.
fn retry_delay(attempt: usize) -> Duration {
    if attempt == 0 {
        Duration::from_millis(100)
    } else {
        Duration::from_millis((((1u64 << attempt) - 1) * 1000) / 2)
    }
}

fn parse_body(response: &HttpResponse) -> Value {
    let text = response.text();
    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text.into_owned()))
}

/// `DefaultTransporter.processError`: the message comes from the body's `error` when it has one.
fn error_message(response: &HttpResponse) -> String {
    let body = parse_body(response);
    if js_truthy(&body) {
        if let Some(error) = body.get("error").filter(|e| js_truthy(e)) {
            if response.status != 200 {
                return match error {
                    Value::String(text) => text.clone(),
                    _ => match error.get("errors").and_then(Value::as_array) {
                        Some(errors) => errors
                            .iter()
                            .map(|e| {
                                e.get("message")
                                    .filter(|m| !m.is_null())
                                    .map_or_else(String::new, js_string)
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                        None => error
                            .get("message")
                            .filter(|m| !m.is_null())
                            .map_or_else(String::new, js_string),
                    },
                };
            }
        }
    }
    if response.status >= 400 {
        return match &body {
            Value::String(text) => text.clone(),
            other => js_string(other),
        };
    }
    format!("Request failed with status code {}", response.status)
}

/// `value?.a?.b`: the nested field when every step is an object holding it.
fn look<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.as_object()?.get(*key))
}

/// An object holding only the fields that are defined, as `JSON.stringify` drops `undefined`.
fn pick(fields: &[(&str, Option<&Value>)]) -> Value {
    Value::Object(
        fields
            .iter()
            .filter_map(|(name, value)| value.map(|v| ((*name).to_string(), v.clone())))
            .collect(),
    )
}

/// `data.items?.[0]`, `None` when it is missing or falsy.
fn first_item(data: &Value) -> Option<&Value> {
    data.get("items")?
        .as_array()?
        .first()
        .filter(|item| js_truthy(item))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Scripted(Mutex<Vec<Result<HttpResponse, TransportError>>>);
    impl Transport for Scripted {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.0.lock().expect("script").remove(0)
        }
    }

    fn reply(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        })
    }

    fn provider(
        script: Vec<Result<HttpResponse, TransportError>>,
    ) -> (YouTubeProvider, Arc<Mutex<Vec<Duration>>>) {
        let creds = Credentials::from_env(
            [("YOUTUBE_API_KEY".to_string(), "k".to_string())]
                .into_iter()
                .collect(),
        );
        let waits = Arc::new(Mutex::new(Vec::new()));
        let seen = waits.clone();
        let p = YouTubeProvider::new(&creds, Arc::new(Scripted(Mutex::new(script))))
            .expect("configured")
            .with_sleeper(Arc::new(move |d| seen.lock().expect("waits").push(d)));
        (p, waits)
    }

    fn query(text: &str) -> Map<String, Value> {
        Map::from_iter([("query".to_string(), json!(text))])
    }

    #[test]
    fn server_errors_are_retried_three_times_with_the_sdk_backoff() {
        let (p, waits) = provider(vec![
            reply(500, "a"),
            reply(500, "b"),
            reply(503, "c"),
            reply(500, "d"),
        ]);
        let err = p.search(&query("x")).expect_err("exhausted");
        assert_eq!(err.message, "d");
        assert_eq!(
            *waits.lock().expect("waits"),
            vec![
                Duration::from_millis(100),
                Duration::from_millis(500),
                Duration::from_millis(1500)
            ]
        );
    }

    #[test]
    fn a_dead_connection_is_retried_twice_then_reported_like_node_fetch() {
        let down = || Err(TransportError::Failed("connect ECONNREFUSED".into()));
        let (p, waits) = provider(vec![down(), down(), down()]);
        let err = p.search(&query("x")).expect_err("down");
        assert!(
            err.message
                .starts_with("request to https://youtube.googleapis.com/youtube/v3/search?"),
            "{}",
            err.message
        );
        assert!(err
            .message
            .ends_with("failed, reason: connect ECONNREFUSED"));
        assert_eq!(waits.lock().expect("waits").len(), 2);
    }

    #[test]
    fn control_a_client_error_is_not_retried() {
        let (p, waits) = provider(vec![reply(404, "no route")]);
        assert_eq!(p.search(&query("x")).expect_err("404").message, "no route");
        assert!(waits.lock().expect("waits").is_empty());
    }

    #[test]
    fn an_unreadable_error_object_without_a_message_reads_as_empty() {
        let (p, _) = provider(vec![reply(400, "{\"error\":{\"code\":400}}")]);
        assert_eq!(p.search(&query("x")).expect_err("400").message, "");
    }
}
