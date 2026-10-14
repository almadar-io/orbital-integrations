//! The `iconify` service: the Rust twin of `IconifyIntegration` (brand-logo and icon resolution via the Iconify
//! API). Actions: `svgExists`, `search`, `getIconBody`. Failures of the HTTP calls are answered as "not found".

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::arxiv::timeout_from;
use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jsnumber::js_number;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};
use crate::uricomponent::encode_uri_component;

pub const SERVICE: &str = "iconify";

const API: &str = "https://api.iconify.design";
const DEFAULT_UA: &str = "Almadar/1.0 (https://almadar.dev)";
const DEFAULT_TIMEOUT_MS: f64 = 6000.0;

pub struct IconifyProvider {
    transport: Arc<dyn Transport>,
    user_agent: String,
    timeout: Duration,
}

/// `iconify` is keyless: the manager configures it from its (all optional) declared credentials.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    integrations.register(SERVICE, Arc::new(IconifyProvider::new(creds, transport)));
}

impl IconifyProvider {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            user_agent: creds
                .get("ICONIFY_USER_AGENT")
                .unwrap_or(DEFAULT_UA)
                .to_string(),
            timeout: timeout_from(creds.get("ICONIFY_TIMEOUT_MS"), DEFAULT_TIMEOUT_MS),
        }
    }

    /// A GET whose every failure (transport, non-2xx) is "no answer", as `headOk` / `getJson` swallow them.
    fn get(&self, url: String) -> Option<String> {
        let response = self
            .transport
            .send(&HttpRequest {
                method: "GET".to_string(),
                url,
                headers: vec![
                    ("Accept".to_string(), "*/*".to_string()),
                    ("User-Agent".to_string(), self.user_agent.clone()),
                ],
                body: Vec::new(),
                timeout: Some(self.timeout),
            })
            .ok()?;
        response.is_success().then(|| response.text().into_owned())
    }

    fn get_json(&self, url: String) -> Option<Value> {
        serde_json::from_str(&self.get(url)?).ok()
    }

    fn svg_exists(&self, params: &Map<String, Value>) -> Value {
        let path = text_param(params, "path");
        if path.is_empty() {
            return json!({ "exists": false });
        }
        json!({ "exists": self.get(format!("{API}/{path}")).is_some() })
    }

    fn search(&self, params: &Map<String, Value>) -> Value {
        let query = text_param(params, "query");
        if query.is_empty() {
            return json!({ "icons": [] });
        }
        let limit = params.get("limit").and_then(Value::as_f64).unwrap_or(1.0);
        let data = self.get_json(format!(
            "{API}/search?query={}&limit={}",
            encode_uri_component(&query),
            js_number(limit)
        ));
        let icons = data
            .and_then(|d| d.get("icons").cloned())
            .filter(|i| !i.is_null())
            .unwrap_or_else(|| json!([]));
        json!({ "icons": icons })
    }

    fn get_icon_body(&self, params: &Map<String, Value>) -> Value {
        let icon_id = text_param(params, "iconId");
        let mut parts = icon_id.split(':');
        let (prefix, name) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if prefix.is_empty() || name.is_empty() {
            return json!({ "body": null });
        }
        let data = self.get_json(format!(
            "{API}/{prefix}.json?icons={}",
            encode_uri_component(name)
        ));
        let body_in = |set: &str| {
            data.as_ref()
                .and_then(|d| d.get(set))
                .and_then(|s| s.get(name))
                .and_then(|i| i.get("body"))
                .filter(|b| !b.is_null())
                .cloned()
        };
        json!({ "body": body_in("icons").or_else(|| body_in("aliases")).unwrap_or(Value::Null) })
    }
}

fn text_param(params: &Map<String, Value>, name: &str) -> String {
    params
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

impl Provider for IconifyProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "svgExists" => Ok(self.svg_exists(params)),
            "search" => Ok(self.search(params)),
            "getIconBody" => Ok(self.get_icon_body(params)),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpResponse, TransportError};

    struct Dead(TransportError);
    impl Transport for Dead {
        fn send(&self, _: &HttpRequest) -> Result<HttpResponse, TransportError> {
            Err(self.0.clone())
        }
    }

    fn provider(error: TransportError) -> IconifyProvider {
        IconifyProvider::new(&Credentials::default(), Arc::new(Dead(error)))
    }

    #[test]
    fn a_dead_connection_or_a_timeout_reads_as_not_found() {
        for error in [
            TransportError::TimedOut,
            TransportError::Failed("refused".into()),
        ] {
            let p = provider(error);
            let params = |k: &str, v: &str| Map::from_iter([(k.to_string(), json!(v))]);
            assert_eq!(
                p.svg_exists(&params("path", "a/b.svg")),
                json!({ "exists": false })
            );
            assert_eq!(p.search(&params("query", "x")), json!({ "icons": [] }));
            assert_eq!(
                p.get_icon_body(&params("iconId", "a:b")),
                json!({ "body": null })
            );
        }
    }
}
