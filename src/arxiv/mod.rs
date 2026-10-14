//! The `arxiv` service: the Rust twin of `ArxivIntegration` (the public Atom API at export.arxiv.org).
//! Action: `search`.

use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jsnumber::js_number;
use crate::jswire::fetch_failure;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};
use crate::uricomponent::encode_uri_component;

pub const SERVICE: &str = "arxiv";

const ENDPOINT: &str = "https://export.arxiv.org/api/query";
const DEFAULT_TIMEOUT_MS: f64 = 10_000.0;
const DEFAULT_MAX_RESULTS: f64 = 8.0;

pub struct ArxivProvider {
    transport: Arc<dyn Transport>,
    timeout: Duration,
}

/// `arxiv` is keyless: the manager configures it from its (all optional) declared credentials.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    integrations.register(SERVICE, Arc::new(ArxivProvider::new(creds, transport)));
}

impl ArxivProvider {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            timeout: timeout_from(creds.get("ARXIV_TIMEOUT_MS"), DEFAULT_TIMEOUT_MS),
        }
    }

    fn search(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return Ok(json!({ "results": [] }));
        }
        let max_results = params
            .get("maxResults")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_MAX_RESULTS);
        let url = format!(
            "{ENDPOINT}?search_query={}&start=0&max_results={}&sortBy=relevance",
            encode_uri_component(&format!("all:{query}")),
            js_number(max_results)
        );
        let response = self
            .transport
            .send(&HttpRequest {
                method: "GET".to_string(),
                url,
                headers: vec![("Accept".to_string(), "application/atom+xml".to_string())],
                body: Vec::new(),
                timeout: Some(self.timeout),
            })
            .map_err(fetch_failure)?;
        if !response.is_success() {
            return Ok(json!({ "results": [] }));
        }
        Ok(json!({ "results": parse_atom(&response.text()) }))
    }
}

/// `setTimeout(_, Number(text))`: an unreadable number fires on the next tick.
pub(crate) fn timeout_from(text: Option<&str>, default_ms: f64) -> Duration {
    let ms = match text {
        None => default_ms,
        Some(t) => t.trim().parse::<f64>().unwrap_or(1.0),
    };
    Duration::from_millis(ms.max(1.0) as u64)
}

impl Provider for ArxivProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "search" => self.search(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

fn tag_text(block: &str, tag: &str, strip: &Regex) -> String {
    let pattern = format!(r"(?i)<{tag}[^>]*>([\s\S]*?)</{tag}>");
    let Ok(re) = Regex::new(&pattern) else {
        return String::new();
    };
    re.captures(block)
        .and_then(|m| m.get(1))
        .map_or_else(String::new, |m| {
            strip.replace_all(m.as_str(), "").trim().to_string()
        })
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The minimal Atom `<entry>` extractor of the TS twin: arXiv's feed is regular, no XML parser.
fn parse_atom(xml: &str) -> Vec<Value> {
    let tags = Regex::new(r"<[^>]+>").expect("tag pattern");
    let author = Regex::new(r"<author>\s*<name>([^<]+)</name>").expect("author pattern");
    let link =
        Regex::new(r#"(?i)<link[^>]*rel="alternate"[^>]*href="([^"]+)""#).expect("link pattern");
    let mut entries = Vec::new();
    for block in xml.split("<entry>").skip(1) {
        let id = tag_text(block, "id", &tags);
        let title = collapse_whitespace(&tag_text(block, "title", &tags));
        let summary = collapse_whitespace(&tag_text(block, "summary", &tags));
        let published = tag_text(block, "published", &tags);
        let authors: Vec<String> = author
            .captures_iter(block)
            .filter_map(|m| m.get(1))
            .map(|m| m.as_str().trim().to_string())
            .collect();
        let url = link
            .captures(block)
            .and_then(|m| m.get(1))
            .map_or_else(|| id.clone(), |m| m.as_str().to_string());
        if !id.is_empty() || !title.is_empty() {
            entries.push(json!({
                "id": id, "title": title, "summary": summary, "authors": authors, "published": published, "url": url,
            }));
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_timeout_fires_on_the_next_tick_and_an_absent_one_takes_the_default() {
        assert_eq!(timeout_from(None, 10_000.0), Duration::from_millis(10_000));
        assert_eq!(
            timeout_from(Some("2500"), 10_000.0),
            Duration::from_millis(2500)
        );
        assert_eq!(
            timeout_from(Some("soon"), 10_000.0),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn an_entry_without_id_or_title_is_dropped_and_one_with_either_is_kept() {
        let feed = "<feed><entry><summary>orphan</summary></entry><entry><title>T</title></entry><entry><id>I</id></entry></feed>";
        let parsed = parse_atom(feed);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["title"], "T");
        assert_eq!(parsed[1]["url"], "I");
    }
}
