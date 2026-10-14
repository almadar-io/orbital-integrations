//! The `email` service: the Rust twin of `EmailIntegration` over `@sendgrid/mail` and `resend`.
//! Action: `send`. SendGrid is chosen when `SENDGRID_API_KEY` is set, Resend otherwise
//! (`RuntimeIntegrationManager`).

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, Transport};

pub const SERVICE: &str = "email";
const SENDGRID_URL: &str = "https://api.sendgrid.com/v3/mail/send";
const RESEND_URL: &str = "https://api.resend.com/emails";
const DEFAULT_FROM: &str = "noreply@example.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    SendGrid,
    Resend,
}

pub struct EmailProvider {
    vendor: Vendor,
    api_key: String,
    from_email: String,
    transport: Arc<dyn Transport>,
}

/// The vendor the credentials select, if any.
pub fn select_vendor(creds: &Credentials) -> Option<Vendor> {
    if creds.get("SENDGRID_API_KEY").is_some() {
        Some(Vendor::SendGrid)
    } else if creds.get("RESEND_API_KEY").is_some() {
        Some(Vendor::Resend)
    } else {
        None
    }
}

pub fn is_configured(creds: &Credentials) -> bool {
    select_vendor(creds).is_some()
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
    match EmailProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

struct Message<'a> {
    to: &'a str,
    subject: &'a str,
    body: &'a str,
    from: String,
    html_body: Option<&'a str>,
    reply_to: Option<&'a str>,
    template_id: Option<&'a str>,
}

impl EmailProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let (vendor, key_name) = match select_vendor(creds) {
            Some(Vendor::SendGrid) => (Vendor::SendGrid, "SENDGRID_API_KEY"),
            Some(Vendor::Resend) => (Vendor::Resend, "RESEND_API_KEY"),
            None => return Err(IntegrationError::plain("SENDGRID_API_KEY not configured")),
        };
        Ok(Self {
            vendor,
            api_key: creds.get(key_name).unwrap_or_default().to_string(),
            from_email: creds.get("FROM_EMAIL").unwrap_or(DEFAULT_FROM).to_string(),
            transport,
        })
    }

    pub fn vendor(&self) -> Vendor {
        self.vendor
    }

    fn send(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let text = |name: &str| params.get(name).and_then(Value::as_str);
        let present = |name: &str| text(name).filter(|v| !v.is_empty());
        let message = Message {
            to: text("to").unwrap_or_default(),
            subject: text("subject").unwrap_or_default(),
            body: text("body").unwrap_or_default(),
            from: present("from").map_or_else(|| self.from_email.clone(), str::to_string),
            html_body: present("htmlBody"),
            reply_to: present("replyTo"),
            template_id: present("templateId"),
        };
        match self.vendor {
            Vendor::SendGrid => self.send_via_sendgrid(&message),
            Vendor::Resend => self.send_via_resend(&message),
        }
    }

    fn send_via_sendgrid(&self, message: &Message) -> Result<Value, IntegrationError> {
        let mut content = Vec::new();
        if message.html_body.is_some() {
            content.push(json!({ "value": message.body, "type": "text/plain" }));
        }
        content.push(
            json!({ "value": message.html_body.unwrap_or(message.body), "type": "text/html" }),
        );
        let mut body = json!({
            "from": address(&message.from),
            "subject": message.subject,
            "personalizations": [{ "to": [address(message.to)] }],
            "content": content,
        });
        if let Some(reply_to) = message.reply_to {
            body["reply_to"] = address(reply_to);
        }
        if let Some(template) = message.template_id {
            body["template_id"] = json!(template);
        }
        let request = HttpRequest {
            method: "POST".into(),
            url: SENDGRID_URL.into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", self.api_key)),
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "application/json".into()),
            ],
            body: body.to_string().into_bytes(),
            timeout: None,
        };
        let response = self
            .transport
            .send(&request)
            .map_err(|e| IntegrationError::plain(transport_detail(e)))?;
        if !(200..300).contains(&response.status) {
            return Err(IntegrationError::plain(response.status_text()));
        }
        let mut out = Map::new();
        if let Some(id) = response.header("x-message-id") {
            out.insert("id".into(), json!(id));
        }
        out.insert("status".into(), json!("sent"));
        Ok(Value::Object(out))
    }

    fn send_via_resend(&self, message: &Message) -> Result<Value, IntegrationError> {
        if message.template_id.is_some() {
            return Err(IntegrationError::plain(
                "templateId is not supported by the resend provider — use sendgrid or drop templateId",
            ));
        }
        let mut body = Map::new();
        body.insert("from".into(), json!(message.from));
        body.insert(
            "html".into(),
            json!(message.html_body.unwrap_or(message.body)),
        );
        if message.html_body.is_some() {
            body.insert("text".into(), json!(message.body));
        }
        if let Some(reply_to) = message.reply_to {
            body.insert("reply_to".into(), json!(reply_to));
        }
        body.insert("subject".into(), json!(message.subject));
        body.insert("to".into(), json!(message.to));
        let request = HttpRequest {
            method: "POST".into(),
            url: RESEND_URL.into(),
            headers: vec![
                ("Authorization".into(), format!("Bearer {}", self.api_key)),
                ("Content-Type".into(), "application/json".into()),
                ("Accept".into(), "*/*".into()),
            ],
            body: Value::Object(body).to_string().into_bytes(),
            timeout: None,
        };
        let (data, error) = match self.transport.send(&request) {
            Err(_) => (None, Some(json!({ "message": RESEND_UNRESOLVED }))),
            Ok(response) if response.is_success() => {
                match serde_json::from_str::<Value>(&response.text()) {
                    Ok(data) => (Some(data), None),
                    Err(_) => (None, Some(json!({ "message": RESEND_UNRESOLVED }))),
                }
            }
            Ok(response) => match serde_json::from_str::<Value>(&response.text()) {
                Ok(error) => (None, Some(error)),
                Err(_) => (None, Some(json!({ "message": RESEND_SERVER_ERROR }))),
            },
        };
        let error_is_truthy = error.as_ref().is_some_and(js_truthy);
        let data = data.filter(js_truthy);
        if error_is_truthy || data.is_none() {
            let reason = error
                .as_ref()
                .and_then(|e| e.get("message"))
                .filter(|m| !m.is_null())
                .map_or_else(|| "no message id returned".to_string(), js_to_string);
            return Err(IntegrationError::plain(format!(
                "Resend rejected the email: {reason}"
            )));
        }
        let mut out = Map::new();
        if let Some(id) = data.as_ref().and_then(|d| d.get("id")) {
            out.insert("id".into(), id.clone());
        }
        out.insert("status".into(), json!("sent"));
        Ok(Value::Object(out))
    }
}

const RESEND_UNRESOLVED: &str = "Unable to fetch data. The request could not be resolved.";
const RESEND_SERVER_ERROR: &str =
    "Internal server error. We are unable to process your request right now, please try again later.";

fn transport_detail(error: crate::transport::TransportError) -> String {
    match error {
        crate::transport::TransportError::TimedOut => "timeout".to_string(),
        crate::transport::TransportError::Failed(detail) => detail,
    }
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `${value}` for the JSON values a vendor error `message` can hold.
fn js_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// `@sendgrid/helpers` `splitNameEmail` + `EmailAddress.toJSON`: `Name <a@b.c>` becomes name and email.
fn address(raw: &str) -> Value {
    if !raw.contains('<') {
        return json!({ "email": raw });
    }
    let mut parts = raw.split('<');
    let name = parts.next().unwrap_or_default().trim();
    let email = parts.next().unwrap_or_default().replacen('>', "", 1);
    let email = email.trim();
    if name.is_empty() {
        json!({ "email": email })
    } else {
        json!({ "email": email, "name": name })
    }
}

impl Provider for EmailProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "send" => self.send(params),
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
    fn addresses_split_like_the_sendgrid_helper() {
        assert_eq!(
            address("ada@example.com"),
            json!({ "email": "ada@example.com" })
        );
        assert_eq!(
            address(" Ada L <ada@example.com> "),
            json!({ "email": "ada@example.com", "name": "Ada L" })
        );
        assert_eq!(
            address("<ada@example.com>"),
            json!({ "email": "ada@example.com" })
        );
    }

    #[test]
    fn truthiness_follows_javascript() {
        for falsy in [json!(null), json!(false), json!(0), json!("")] {
            assert!(!js_truthy(&falsy), "{falsy}");
        }
        for truthy in [json!({}), json!([]), json!("x"), json!(1)] {
            assert!(js_truthy(&truthy), "{truthy}");
        }
    }
}
