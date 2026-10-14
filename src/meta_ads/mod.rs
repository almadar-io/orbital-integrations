//! The `metaAds` service: the Rust twin of `MetaAdsIntegration` (Graph API Marketing Insights, read-only).
//! Actions: `getSpend`, `listCampaigns`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jsnumber::js_to_number;
use crate::json::number;
use crate::jswire::fetch_failure;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};
use crate::uricomponent::form_urlencode;

pub const SERVICE: &str = "metaAds";

const GRAPH_BASE: &str = "https://graph.facebook.com/v21.0";
const TIMEOUT: Duration = Duration::from_millis(15_000);

pub struct MetaAdsProvider {
    transport: Arc<dyn Transport>,
    access_token: String,
    default_account_id: String,
}

/// Configured when `META_ACCESS_TOKEN` is present, as the manager does.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    match MetaAdsProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(_) => integrations.register_unconfigured(SERVICE),
    }
}

impl MetaAdsProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let access_token = creds.get("META_ACCESS_TOKEN").unwrap_or("").to_string();
        if access_token.is_empty() {
            return Err(IntegrationError::plain("META_ACCESS_TOKEN not configured"));
        }
        Ok(Self {
            transport,
            access_token,
            default_account_id: creds.get("META_AD_ACCOUNT_ID").unwrap_or("").to_string(),
        })
    }

    fn account_id(&self, params: &Map<String, Value>) -> Result<String, IntegrationError> {
        let given = params
            .get("accountId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = if given.is_empty() {
            self.default_account_id.as_str()
        } else {
            given
        };
        if id.is_empty() {
            return Err(IntegrationError::plain(
                "No ad account: pass `accountId` or set META_AD_ACCOUNT_ID",
            ));
        }
        Ok(if id.starts_with("act_") {
            id.to_string()
        } else {
            format!("act_{id}")
        })
    }

    fn graph_get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, IntegrationError> {
        let mut url = format!("{GRAPH_BASE}/{}", encode_path(path));
        let mut separator = '?';
        for (key, value) in query
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .chain([("access_token", self.access_token.as_str())])
        {
            url.push(separator);
            url.push_str(&format!(
                "{}={}",
                form_urlencode(key),
                form_urlencode(value)
            ));
            separator = '&';
        }
        let response = self
            .transport
            .send(&HttpRequest {
                method: "GET".to_string(),
                url,
                headers: vec![("Accept".to_string(), "*/*".to_string())],
                body: Vec::new(),
                timeout: Some(TIMEOUT),
            })
            .map_err(fetch_failure)?;
        if response.status >= 500 {
            return Err(IntegrationError::plain(format!(
                "Meta Graph API returned {}",
                response.status
            )));
        }
        let body: Value = serde_json::from_str(&response.text())
            .map_err(|e| IntegrationError::plain(e.to_string()))?;
        if !response.is_success() {
            let reason = body
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map_or_else(|| response.status.to_string(), str::to_string);
            return Err(IntegrationError::plain(format!(
                "Meta Graph API error: {reason}"
            )));
        }
        Ok(match body.get("data") {
            Some(data) if !data.is_null() => data.clone(),
            _ => body,
        })
    }

    fn get_spend(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let since =
            serde_json::to_string(params.get("since").unwrap_or(&Value::Null)).unwrap_or_default();
        let until =
            serde_json::to_string(params.get("until").unwrap_or(&Value::Null)).unwrap_or_default();
        let rows = self.graph_get(
            &format!("{}/insights", self.account_id(params)?),
            &[
                (
                    "fields",
                    "spend,impressions,clicks,account_currency".to_string(),
                ),
                (
                    "time_range",
                    format!("{{\"since\":{since},\"until\":{until}}}"),
                ),
                ("level", "account".to_string()),
            ],
        )?;
        let row = rows.as_array().and_then(|r| r.first());
        let field = |name: &str| row.and_then(|r| r.get(name)).filter(|v| !v.is_null());
        let count = |name: &str| number(field(name).map_or(0.0, js_to_number));
        Ok(json!({
            "spend": count("spend"),
            "currency": field("account_currency").cloned().unwrap_or_else(|| json!("")),
            "impressions": count("impressions"),
            "clicks": count("clicks"),
        }))
    }

    fn list_campaigns(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let mut query = vec![("fields", "id,name,status,daily_budget".to_string())];
        if let Some(status) = params
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            query.push((
                "effective_status",
                serde_json::to_string(&[status]).unwrap_or_default(),
            ));
        }
        let rows = self.graph_get(&format!("{}/campaigns", self.account_id(params)?), &query)?;
        let campaigns: Vec<Value> = rows
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        let field = |name: &str| row.get(name).filter(|v| !v.is_null()).cloned();
                        json!({
                            "id": field("id").unwrap_or_else(|| json!("")),
                            "name": field("name").unwrap_or_else(|| json!("")),
                            "status": field("status").unwrap_or_else(|| json!("")),
                            "dailyBudget": number(field("daily_budget").map_or(0.0, |v| js_to_number(&v)) / 100.0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "campaigns": campaigns }))
    }
}

/// The URL path percent-encode set `new URL(...)` applies: space, `"`, `<`, `>`, `` ` ``, `{`, `}` and non-ASCII.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for &byte in path.as_bytes() {
        if byte <= 0x20 || byte >= 0x7f || b"\"<>`{}".contains(&byte) {
            out.push_str(&format!("%{byte:02X}"));
        } else {
            out.push(byte as char);
        }
    }
    out
}

impl Provider for MetaAdsProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "getSpend" => self.get_spend(params),
            "listCampaigns" => self.list_campaigns(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_encoding_follows_the_url_parser_set() {
        assert_eq!(encode_path("act_1/insights"), "act_1/insights");
        assert_eq!(encode_path("act_a b\"é"), "act_a%20b%22%C3%A9");
    }
}
