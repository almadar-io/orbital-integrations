//! The `wikimedia` service: the Rust twin of `WikimediaIntegration` (redirect-resolved Wikipedia page lookup).
//! Action: `getPage`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::arxiv::timeout_from;
use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jswire::{fetch_failure, js_slice};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};
use crate::uricomponent::encode_uri_component;

pub const SERVICE: &str = "wikimedia";

const ENDPOINT: &str = "https://en.wikipedia.org/w/api.php";
const DEFAULT_UA: &str = "Almadar/1.0 (https://almadar.dev)";
const DEFAULT_TIMEOUT_MS: f64 = 6000.0;
const EXTRACT_CHAR_CAP: usize = 2000;

pub struct WikimediaProvider {
    transport: Arc<dyn Transport>,
    user_agent: String,
    timeout: Duration,
}

/// `wikimedia` is keyless: the manager configures it from its (all optional) declared credentials.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    integrations.register(SERVICE, Arc::new(WikimediaProvider::new(creds, transport)));
}

impl WikimediaProvider {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            user_agent: creds
                .get("WIKIMEDIA_USER_AGENT")
                .unwrap_or(DEFAULT_UA)
                .to_string(),
            timeout: timeout_from(creds.get("WIKIMEDIA_TIMEOUT_MS"), DEFAULT_TIMEOUT_MS),
        }
    }

    fn get_page(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if title.is_empty() {
            return Ok(Value::Object(Map::new()));
        }
        let url = format!(
            "{ENDPOINT}?action=query&format=json&redirects=1&prop=pageimages|description|extracts&explaintext=1&exintro=1&piprop=thumbnail&pithumbsize=240&titles={}",
            encode_uri_component(title)
        );
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
            .map_err(fetch_failure)?;
        if !response.is_success() {
            return Ok(Value::Object(Map::new()));
        }
        let data: Value = serde_json::from_str(&response.text())
            .map_err(|e| IntegrationError::plain(e.to_string()))?;
        Ok(read_page(&data))
    }
}

impl Provider for WikimediaProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "getPage" => self.get_page(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

/// `Object.values(pages)[0]`: integer-like keys come first in ascending order, as in JS.
fn first_page(pages: &Map<String, Value>) -> Option<&Value> {
    let mut indexed: Vec<(u32, &Value)> = pages
        .iter()
        .filter_map(|(k, v)| {
            let n = k.parse::<u32>().ok()?;
            (n.to_string() == *k && n != u32::MAX).then_some((n, v))
        })
        .collect();
    indexed.sort_by_key(|(n, _)| *n);
    indexed
        .first()
        .map(|(_, v)| *v)
        .or_else(|| pages.values().next())
}

fn read_page(data: &Value) -> Value {
    let Some(Value::Object(pages)) = data.get("query").and_then(|q| q.get("pages")) else {
        return Value::Object(Map::new());
    };
    let Some(Value::Object(page)) = first_page(pages) else {
        return Value::Object(Map::new());
    };
    if page.contains_key("missing") {
        return Value::Object(Map::new());
    }
    let extract_text = page.get("extract").and_then(Value::as_str);
    let extract = extract_text.map(|t| js_slice(t, EXTRACT_CHAR_CAP).trim().to_string());
    let description = match page.get("description") {
        Some(v) if !v.is_null() => Some(v.clone()),
        _ => extract_text.map(|t| Value::String(t.split('.').next().unwrap_or("").to_string())),
    };
    let portrait = page.get("thumbnail").and_then(|t| t.get("source")).cloned();
    let mut out = Map::new();
    if let Some(title) = page.get("title") {
        out.insert("title".to_string(), title.clone());
    }
    if let Some(description) = description {
        out.insert("description".to_string(), description);
    }
    if let Some(portrait) = portrait {
        out.insert("portraitUrl".to_string(), portrait);
    }
    if let Some(extract) = extract {
        out.insert("extract".to_string(), Value::String(extract));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn integer_page_ids_come_first_in_ascending_order() {
        let pages =
            json!({ "b": {"title": "named"}, "20": {"title": "twenty"}, "3": {"title": "three"} });
        let Value::Object(map) = pages else {
            unreachable!()
        };
        assert_eq!(
            first_page(&map).and_then(|p| p.get("title")),
            Some(&json!("three"))
        );
    }

    #[test]
    fn control_a_non_index_key_is_still_found_when_no_page_id_exists() {
        let Value::Object(map) = json!({ "-1": {"title": "missing"} }) else {
            unreachable!()
        };
        assert_eq!(
            first_page(&map).and_then(|p| p.get("title")),
            Some(&json!("missing"))
        );
    }

    #[test]
    fn a_description_of_null_falls_back_to_the_first_sentence() {
        let page = json!({ "query": { "pages": { "1": { "title": "T", "description": null, "extract": "One. Two." } } } });
        assert_eq!(read_page(&page)["description"], json!("One"));
    }
}
