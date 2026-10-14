//! The `analytics` service: the Rust twin of the browser `createAnalyticsService` (one `@almadar/analytics`
//! beacon per call). Actions: `pageview`, `track`. The page the visit came from is a port; with none the host
//! reads as local and nothing is sent, exactly as the TS service behaves outside a browser.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::{IntegrationError, VALIDATION_ERROR};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};

pub const SERVICE: &str = "analytics";

/// What the host knows about the visit (`AnalyticsPagePort`).
pub trait PagePort: Send + Sync {
    fn hostname(&self) -> String;
    fn referrer(&self) -> String;
    fn screen_width(&self) -> Option<f64>;
}

/// No page: an empty host, which the beacon treats as local.
pub struct NoPage;

impl PagePort for NoPage {
    fn hostname(&self) -> String {
        String::new()
    }
    fn referrer(&self) -> String {
        String::new()
    }
    fn screen_width(&self) -> Option<f64> {
        None
    }
}

pub struct AnalyticsProvider {
    transport: Arc<dyn Transport>,
    page: Arc<dyn PagePort>,
}

/// `analytics` is keyless and always available; without a page port it never sends.
pub fn register(
    integrations: &mut Integrations,
    _creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    integrations.register(SERVICE, Arc::new(AnalyticsProvider::new(transport)));
}

impl AnalyticsProvider {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            page: Arc::new(NoPage),
        }
    }

    pub fn with_page(mut self, page: Arc<dyn PagePort>) -> Self {
        self.page = page;
        self
    }

    fn post(&self, endpoint: &str, payload: Value) -> Value {
        if endpoint.is_empty() || is_localhost_host(&self.page.hostname()) {
            return json!({ "sent": false });
        }
        let outcome = self.transport.send(&HttpRequest {
            method: "POST".to_string(),
            url: endpoint.to_string(),
            headers: vec![
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("Accept".to_string(), "*/*".to_string()),
            ],
            body: payload.to_string().into_bytes(),
            timeout: None,
        });
        json!({ "sent": outcome.is_ok_and(|r| r.is_success()) })
    }

    fn domain(&self, params: &Map<String, Value>) -> Result<String, IntegrationError> {
        let given = opt_str(params, "domain")?;
        Ok(match given {
            Some(d) if !d.is_empty() => d,
            _ => self.page.hostname(),
        })
    }
}

impl Provider for AnalyticsProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "pageview" => {
                let endpoint = str_param(params, "endpoint")?;
                let path = path_param(params)?;
                let referrer = self.page.referrer();
                let mut payload = Map::new();
                payload.insert("n".into(), json!("pageview"));
                payload.insert("d".into(), json!(self.domain(params)?));
                payload.insert("u".into(), json!(path));
                if !referrer.is_empty() {
                    payload.insert("r".into(), json!(referrer));
                }
                if let Some(width) = self.page.screen_width() {
                    payload.insert("w".into(), crate::json::number(width));
                }
                Ok(self.post(&endpoint, Value::Object(payload)))
            }
            "track" => {
                let endpoint = str_param(params, "endpoint")?;
                let name = str_param(params, "name")?;
                if name == "pageview" {
                    return Err(validation(
                        "Event name 'pageview' is reserved; call pageview".to_string(),
                    ));
                }
                let path = path_param(params)?;
                let props = scalar_map(params, "props")?;
                let mut payload = Map::new();
                payload.insert("n".into(), json!(name));
                payload.insert("d".into(), json!(self.domain(params)?));
                payload.insert("u".into(), json!(path));
                if let Some(props) = props {
                    payload.insert("p".into(), Value::Object(props));
                }
                Ok(self.post(&endpoint, Value::Object(payload)))
            }
            other => Err(validation(format!("analytics has no action '{other}'"))),
        }
    }
}

fn validation(message: String) -> IntegrationError {
    IntegrationError::coded(VALIDATION_ERROR, message)
}

fn invalid(name: &str, expected: &str) -> IntegrationError {
    validation(format!("Param '{name}' must be {expected}"))
}

fn str_param(params: &Map<String, Value>, name: &str) -> Result<String, IntegrationError> {
    match params.get(name) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(invalid(name, "a string")),
    }
}

fn opt_str(params: &Map<String, Value>, name: &str) -> Result<Option<String>, IntegrationError> {
    match params.get(name) {
        None => Ok(None),
        Some(_) => str_param(params, name).map(Some),
    }
}

fn path_param(params: &Map<String, Value>) -> Result<String, IntegrationError> {
    let path = str_param(params, "path")?;
    if path.starts_with('/') {
        Ok(path)
    } else {
        Err(validation(format!(
            "Param 'path' must start with \"/\", got '{path}'"
        )))
    }
}

fn scalar_map(
    params: &Map<String, Value>,
    name: &str,
) -> Result<Option<Map<String, Value>>, IntegrationError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    let Value::Object(map) = value else {
        return Err(invalid(name, "a map of name to string, number or boolean"));
    };
    let mut out = Map::new();
    for (key, item) in map {
        match item {
            Value::String(_) | Value::Bool(_) | Value::Number(_) => {
                out.insert(key.clone(), item.clone());
            }
            _ => {
                return Err(invalid(
                    &format!("{name}.{key}"),
                    "a string, number or boolean",
                ))
            }
        }
    }
    Ok(Some(out))
}

/// `isLocalhostHost` of `@almadar/analytics`.
pub fn is_localhost_host(hostname: &str) -> bool {
    matches!(
        hostname,
        "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "[::1]" | ""
    ) || hostname.ends_with(".local")
        || hostname.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpResponse, TransportError};

    struct Dead;
    impl Transport for Dead {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse, TransportError> {
            Err(TransportError::Failed("refused".into()))
        }
    }
    struct Host(&'static str);
    impl PagePort for Host {
        fn hostname(&self) -> String {
            self.0.to_string()
        }
        fn referrer(&self) -> String {
            String::new()
        }
        fn screen_width(&self) -> Option<f64> {
            None
        }
    }

    fn params(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn a_dead_collector_reads_as_not_sent_and_a_page_less_host_never_posts() {
        let live =
            AnalyticsProvider::new(Arc::new(Dead)).with_page(Arc::new(Host("app.example.com")));
        let call = params(&[("endpoint", json!("http://c/e")), ("path", json!("/"))]);
        assert_eq!(
            live.call("pageview", &call, &CallContext::anonymous())
                .expect("ok"),
            json!({ "sent": false })
        );
        let none = AnalyticsProvider::new(Arc::new(Dead));
        assert_eq!(
            none.call("pageview", &call, &CallContext::anonymous())
                .expect("ok"),
            json!({ "sent": false })
        );
    }

    #[test]
    fn the_provider_checks_its_own_param_types_like_the_ts_service() {
        let p = AnalyticsProvider::new(Arc::new(Dead));
        let err = p
            .call(
                "pageview",
                &params(&[("endpoint", json!(1)), ("path", json!("/"))]),
                &CallContext::anonymous(),
            )
            .expect_err("type");
        assert_eq!(err.message, "Param 'endpoint' must be a string");
        let err = p
            .call(
                "track",
                &params(&[
                    ("endpoint", json!("e")),
                    ("name", json!("n")),
                    ("path", json!("/")),
                    ("props", json!({"a": {"b": 1}})),
                ]),
                &CallContext::anonymous(),
            )
            .expect_err("nested");
        assert_eq!(
            err.message,
            "Param 'props.a' must be a string, number or boolean"
        );
        let err = p
            .call("identify", &Map::new(), &CallContext::anonymous())
            .expect_err("unknown");
        assert_eq!(err.message, "analytics has no action 'identify'");
        assert_eq!(err.code.as_deref(), Some(VALIDATION_ERROR));
    }

    #[test]
    fn local_hosts_are_recognized_and_a_lookalike_public_host_is_not() {
        for host in [
            "localhost",
            "127.0.0.1",
            "0.0.0.0",
            "::1",
            "[::1]",
            "",
            "a.local",
            "b.localhost",
        ] {
            assert!(is_localhost_host(host), "{host}");
        }
        assert!(!is_localhost_host("localhost.example.com"));
    }
}
