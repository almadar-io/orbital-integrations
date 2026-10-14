//! The `banking` service: the Rust twin of `BankingIntegration` (GoCardless Bank Account Data).
//! Actions: `createRequisition`, `listAccounts`, `listTransactions`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::json::number;
use crate::jswire::{
    fetch_failure, fetch_url, js_number, js_slice, json_restringify, system_clock, Clock,
};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};

pub const SERVICE: &str = "banking";
const GC_BASE: &str = "https://bankaccountdata.gocardless.com/api/v2";
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const EXPIRY_MARGIN_MS: u64 = 60_000;

struct Token {
    access: String,
    expires_at: u64,
}

pub struct BankingProvider {
    secret_id: String,
    secret_key: String,
    transport: Arc<dyn Transport>,
    clock: Clock,
    token: Mutex<Option<Token>>,
}

/// The credentials that configure `banking` (`RuntimeIntegrationManager`).
pub fn is_configured(creds: &Credentials) -> bool {
    creds.get("GOCARDLESS_SECRET_ID").is_some() && creds.get("GOCARDLESS_SECRET_KEY").is_some()
}

pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    if !is_configured(creds) {
        integrations.register_unconfigured(SERVICE);
        return;
    }
    match BankingProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl BankingProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let (Some(id), Some(key)) = (
            creds.get("GOCARDLESS_SECRET_ID"),
            creds.get("GOCARDLESS_SECRET_KEY"),
        ) else {
            return Err(IntegrationError::plain(
                "GOCARDLESS_SECRET_ID / GOCARDLESS_SECRET_KEY not configured",
            ));
        };
        Ok(Self {
            secret_id: id.to_string(),
            secret_key: key.to_string(),
            transport,
            clock: system_clock(),
            token: Mutex::new(None),
        })
    }

    /// Replace the clock the access-token expiry reads (tests; hosts with their own).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn access_token(&self) -> Result<String, IntegrationError> {
        let now = (self.clock)();
        let mut cached = self.token.lock().expect("token lock");
        if let Some(token) = cached.as_ref() {
            if now < token.expires_at.saturating_sub(EXPIRY_MARGIN_MS) {
                return Ok(token.access.clone());
            }
        }
        let request = HttpRequest {
            method: "POST".into(),
            url: format!("{GC_BASE}/token/new/"),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "application/json".into()),
            ],
            body: json!({ "secret_id": self.secret_id, "secret_key": self.secret_key })
                .to_string()
                .into_bytes(),
            timeout: Some(TOKEN_TIMEOUT),
        };
        let response = self.transport.send(&request).map_err(fetch_failure)?;
        if !response.is_success() {
            return Err(IntegrationError::plain(format!(
                "GoCardless token request failed: {}",
                response.status
            )));
        }
        let body: Value = serde_json::from_str(&response.text())
            .map_err(|e| IntegrationError::plain(format!("Unexpected token in JSON: {e}")))?;
        let access = match body.get("access") {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        let expires = js_number(body.get("access_expires"));
        *cached = Some(Token {
            access: access.clone(),
            expires_at: now.saturating_add((expires * 1000.0).max(0.0) as u64),
        });
        Ok(access)
    }

    fn request(
        &self,
        path: &str,
        method: &str,
        body: Option<&Value>,
    ) -> Result<Value, IntegrationError> {
        let token = self.access_token()?;
        let request = HttpRequest {
            method: method.to_string(),
            url: fetch_url(&format!("{GC_BASE}{path}")),
            headers: vec![
                ("Accept".into(), "application/json".into()),
                ("Content-Type".into(), "application/json".into()),
                ("Authorization".into(), format!("Bearer {token}")),
            ],
            body: body.map(Value::to_string).unwrap_or_default().into_bytes(),
            timeout: Some(REQUEST_TIMEOUT),
        };
        let response = self.transport.send(&request).map_err(fetch_failure)?;
        if response.status >= 500 {
            return Err(IntegrationError::plain(format!(
                "GoCardless returned {}",
                response.status
            )));
        }
        let parsed: Value = serde_json::from_str(&response.text())
            .map_err(|e| IntegrationError::plain(format!("Unexpected token in JSON: {e}")))?;
        if !response.is_success() {
            let written = json_restringify(&response.text()).unwrap_or_default();
            return Err(IntegrationError::plain(format!(
                "GoCardless error {}: {}",
                response.status,
                js_slice(&written, 300)
            )));
        }
        Ok(parsed)
    }

    fn create_requisition(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let mut body = Map::new();
        body.insert(
            "institution_id".into(),
            params.get("institutionId").cloned().unwrap_or(Value::Null),
        );
        body.insert(
            "redirect".into(),
            params.get("redirectUrl").cloned().unwrap_or(Value::Null),
        );
        if let Some(reference) = params
            .get("reference")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            body.insert("reference".into(), Value::String(reference.to_string()));
        }
        let reply = self.request("/requisitions/", "POST", Some(&Value::Object(body)))?;
        Ok(json!({
            "requisitionId": or_empty(&reply, "id"),
            "link": or_empty(&reply, "link"),
        }))
    }

    fn list_accounts(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = text(params, "requisitionId");
        let reply = self.request(&format!("/requisitions/{id}/"), "GET", None)?;
        Ok(json!({ "accounts": non_null(&reply, "accounts").unwrap_or_else(|| json!([])) }))
    }

    fn list_transactions(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = text(params, "accountId");
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (param, key) in [("dateFrom", "date_from"), ("dateTo", "date_to")] {
            let value = text(params, param);
            if !value.is_empty() {
                query.append_pair(key, &value);
            }
        }
        let query = query.finish();
        let suffix = if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        };
        let reply = self.request(
            &format!("/accounts/{id}/transactions/{suffix}"),
            "GET",
            None,
        )?;
        let booked = reply
            .get("transactions")
            .and_then(|t| t.get("booked"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let transactions: Vec<Value> = booked.iter().map(shape_transaction).collect();
        Ok(json!({ "transactions": transactions }))
    }
}

fn text(params: &Map<String, Value>, name: &str) -> String {
    params
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn non_null(value: &Value, field: &str) -> Option<Value> {
    value.get(field).filter(|v| !v.is_null()).cloned()
}

/// `value[field] ?? ''`.
fn or_empty(value: &Value, field: &str) -> Value {
    non_null(value, field).unwrap_or_else(|| json!(""))
}

fn shape_transaction(tx: &Value) -> Value {
    let amount = tx.get("transactionAmount");
    let first = |fields: &[&str]| -> Value {
        fields
            .iter()
            .find_map(|f| non_null(tx, f))
            .unwrap_or_else(|| json!(""))
    };
    json!({
        "id": first(&["transactionId", "internalTransactionId"]),
        "amount": number(js_number(amount.and_then(|a| a.get("amount")))),
        "currency": amount.map_or_else(|| json!(""), |a| or_empty(a, "currency")),
        "date": or_empty(tx, "bookingDate"),
        "description": or_empty(tx, "remittanceInformationUnstructured"),
        "counterparty": first(&["creditorName", "debtorName"]),
    })
}

impl Provider for BankingProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "createRequisition" => self.create_requisition(params),
            "listAccounts" => self.list_accounts(params),
            "listTransactions" => self.list_transactions(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}
