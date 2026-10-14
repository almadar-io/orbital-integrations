//! The `esign` service: the Rust twin of `EsignIntegration` (DocuSign eSignature REST).
//! Actions: `sendEnvelope`, `getEnvelopeStatus`, `downloadDocument`.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jswire::{base64_encode, fetch_failure, fetch_url, js_slice};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};

pub const SERVICE: &str = "esign";
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct EsignProvider {
    creds: Credentials,
    transport: Arc<dyn Transport>,
}

/// The credentials that configure `esign` (`RuntimeIntegrationManager`).
pub fn is_configured(creds: &Credentials) -> bool {
    creds.get("DOCUSIGN_BASE_URL").is_some() && creds.get("DOCUSIGN_ACCESS_TOKEN").is_some()
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
    match EsignProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl EsignProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        if !is_configured(creds) {
            return Err(IntegrationError::plain(
                "DOCUSIGN_BASE_URL / DOCUSIGN_ACCESS_TOKEN not configured",
            ));
        }
        Ok(Self {
            creds: creds.clone(),
            transport,
        })
    }

    fn request(
        &self,
        path: &str,
        method: &str,
        body: Option<&Value>,
        raw: bool,
    ) -> Result<Vec<u8>, IntegrationError> {
        let base = self.creds.get("DOCUSIGN_BASE_URL").unwrap_or_default();
        let base = base.strip_suffix('/').unwrap_or(base);
        let token = self.creds.get("DOCUSIGN_ACCESS_TOKEN").unwrap_or_default();
        let request = HttpRequest {
            method: method.to_string(),
            url: fetch_url(&format!("{base}{path}")),
            headers: vec![
                (
                    "Accept".into(),
                    if raw {
                        "application/pdf"
                    } else {
                        "application/json"
                    }
                    .into(),
                ),
                ("Content-Type".into(), "application/json".into()),
                ("Authorization".into(), format!("Bearer {token}")),
            ],
            body: body.map(Value::to_string).unwrap_or_default().into_bytes(),
            timeout: Some(TIMEOUT),
        };
        let response = self.transport.send(&request).map_err(fetch_failure)?;
        if response.status >= 500 {
            return Err(IntegrationError::plain(format!(
                "DocuSign returned {}",
                response.status
            )));
        }
        if !response.is_success() {
            return Err(IntegrationError::plain(format!(
                "DocuSign error {}: {}",
                response.status,
                js_slice(&response.text(), 300)
            )));
        }
        Ok(response.body)
    }

    fn json_request(
        &self,
        path: &str,
        method: &str,
        body: Option<&Value>,
    ) -> Result<Value, IntegrationError> {
        let reply = self.request(path, method, body, false)?;
        serde_json::from_str(&String::from_utf8_lossy(&reply))
            .map_err(|e| IntegrationError::plain(format!("Unexpected token in JSON: {e}")))
    }

    fn send_envelope(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let text = |name: &str| params.get(name).and_then(Value::as_str).unwrap_or_default();
        let content = text("documentContent");
        let base64 = if content.starts_with("data:") {
            content.find(',').map_or(content, |i| &content[i + 1..])
        } else {
            content
        };
        let document_name = text("documentName");
        let subject = match text("emailSubject") {
            "" => format!("Please sign: {document_name}"),
            given => given.to_string(),
        };
        let extension = match document_name.rsplit('.').next() {
            Some("") | None => "pdf",
            Some(ext) => ext,
        };
        let body = json!({
            "emailSubject": subject,
            "status": "sent",
            "documents": [{
                "documentBase64": base64,
                "name": document_name,
                "fileExtension": extension,
                "documentId": "1",
            }],
            "recipients": { "signers": [{
                "email": text("recipientEmail"),
                "name": text("recipientName"),
                "recipientId": "1",
                "routingOrder": "1",
            }]},
        });
        let reply = self.json_request("/envelopes", "POST", Some(&body))?;
        Ok(json!({
            "envelopeId": str_or(&reply, "envelopeId", ""),
            "status": str_or(&reply, "status", "sent"),
        }))
    }

    fn get_envelope_status(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = params
            .get("envelopeId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let reply = self.json_request(&format!("/envelopes/{id}"), "GET", None)?;
        Ok(json!({
            "status": str_or(&reply, "status", ""),
            "completedAt": str_or(&reply, "completedDateTime", ""),
        }))
    }

    fn download_document(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = params
            .get("envelopeId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let body = self.request(
            &format!("/envelopes/{id}/documents/combined"),
            "GET",
            None,
            true,
        )?;
        Ok(json!({
            "content": base64_encode(&body),
            "documentName": format!("envelope-{id}.pdf"),
        }))
    }
}

/// `body.field ?? fallback` for a string field.
fn str_or(value: &Value, field: &str, fallback: &str) -> String {
    match value.get(field) {
        Some(Value::String(s)) => s.clone(),
        _ => fallback.to_string(),
    }
}

impl Provider for EsignProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "sendEnvelope" => self.send_envelope(params),
            "getEnvelopeStatus" => self.get_envelope_status(params),
            "downloadDocument" => self.download_document(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}
