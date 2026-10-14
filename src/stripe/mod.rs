//! The `stripe` service: the Rust twin of `StripeIntegration` over `stripe-node` 17.x (API version
//! `2025-02-24.acacia`). Actions: `createPaymentIntent`, `confirmPayment`, `refund`, `createCustomer`,
//! `createCheckoutSession`, `createPortalSession`, `getSubscription`, `createSubscription`,
//! `updateSubscription`, `cancelSubscription`.

mod form;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use self::form::{
    encode_component, encode_path_segment, flatten, idempotency_key, iso_from_unix, Pairs,
};
use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::jswire::js_number;
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};

pub const SERVICE: &str = "stripe";
const API_BASE: &str = "https://api.stripe.com";
const API_VERSION: &str = "2025-02-24.acacia";
const MAX_NETWORK_RETRIES: usize = 2;
const TIMEOUT: Duration = Duration::from_millis(80_000);
const RETRY_DELAY: Duration = Duration::from_millis(500);

type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

pub struct StripeProvider {
    api_key: String,
    price_solo: String,
    price_teams: String,
    transport: Arc<dyn Transport>,
    sleeper: Sleeper,
}

/// The credentials that configure `stripe` (`RuntimeIntegrationManager`).
pub fn is_configured(creds: &Credentials) -> bool {
    creds.get("STRIPE_SECRET_KEY").is_some()
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
    match StripeProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl StripeProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let Some(key) = creds.get("STRIPE_SECRET_KEY") else {
            return Err(IntegrationError::plain("STRIPE_SECRET_KEY not configured"));
        };
        Ok(Self {
            api_key: key.to_string(),
            price_solo: creds
                .get("STRIPE_PRICE_SOLO")
                .unwrap_or_default()
                .to_string(),
            price_teams: creds
                .get("STRIPE_PRICE_TEAMS")
                .unwrap_or_default()
                .to_string(),
            transport,
            sleeper: Arc::new(std::thread::sleep),
        })
    }

    /// Replace the wait between network retries (tests; hosts with their own clock).
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    /// One stripe-node request: form-encoded body, version pin, idempotency key on POST, retries on
    /// connection failures, 409 and 5xx unless the API says `stripe-should-retry: false`.
    fn api(&self, method: &str, path: &str, pairs: &Pairs) -> Result<Value, IntegrationError> {
        let mut headers = vec![
            ("Accept".to_string(), "application/json".to_string()),
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            (
                "User-Agent".to_string(),
                format!(
                    "Stripe/v1 OrbitalIntegrations/{}",
                    env!("CARGO_PKG_VERSION")
                ),
            ),
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            ),
            ("Stripe-Version".to_string(), API_VERSION.to_string()),
        ];
        if method == "POST" {
            headers.push(("Idempotency-Key".to_string(), idempotency_key()));
        }
        let request = HttpRequest {
            method: method.to_string(),
            url: format!("{API_BASE}{path}"),
            headers,
            body: pairs.encode().into_bytes(),
            timeout: Some(TIMEOUT),
        };
        let mut retries = 0usize;
        loop {
            match self.transport.send(&request) {
                Err(error) => {
                    if retries < MAX_NETWORK_RETRIES {
                        (self.sleeper)(RETRY_DELAY);
                        retries += 1;
                        continue;
                    }
                    return Err(IntegrationError::plain(match error {
                        TransportError::TimedOut => format!(
                            "Request aborted due to timeout being reached ({}ms)",
                            TIMEOUT.as_millis()
                        ),
                        TransportError::Failed(_) => connection_message(retries),
                    }));
                }
                Ok(response) => {
                    if should_retry(&response, retries) {
                        (self.sleeper)(RETRY_DELAY);
                        retries += 1;
                        continue;
                    }
                    return read_response(&response);
                }
            }
        }
    }

    fn price_for(&self, tier: &str) -> Result<&str, IntegrationError> {
        let price = if tier == "solo" {
            &self.price_solo
        } else {
            &self.price_teams
        };
        if price.is_empty() {
            return Err(IntegrationError::plain(format!(
                "STRIPE_PRICE_{} not configured",
                tier.to_uppercase()
            )));
        }
        Ok(price)
    }

    fn tier_of(&self, price_id: &str) -> &'static str {
        if price_id == self.price_solo {
            "solo"
        } else if price_id == self.price_teams {
            "teams"
        } else {
            ""
        }
    }

    fn shape_subscription(&self, sub: &Value) -> Result<Value, IntegrationError> {
        let first = sub
            .get("items")
            .and_then(|i| i.get("data"))
            .and_then(Value::as_array)
            .and_then(|d| d.first());
        let price_id = first
            .and_then(|i| i.get("price"))
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let quantity = first
            .and_then(|i| i.get("quantity"))
            .filter(|q| !q.is_null())
            .cloned()
            .unwrap_or_else(|| json!(1));
        let customer = match sub.get("customer") {
            Some(Value::String(id)) => id.clone(),
            Some(other) => other
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            None => String::new(),
        };
        let status = match sub.get("status").and_then(Value::as_str) {
            Some(
                s @ ("active" | "past_due" | "canceled" | "incomplete" | "incomplete_expired"
                | "trialing" | "unpaid"),
            ) => s,
            Some("paused") => "unpaid",
            _ => "incomplete",
        };
        Ok(json!({
            "subscriptionId": sub.get("id").cloned().unwrap_or(Value::Null),
            "customerId": customer,
            "status": status,
            "priceId": price_id,
            "quantity": quantity,
            "currentPeriodStart": iso_from_unix(sub.get("current_period_start"))?,
            "currentPeriodEnd": iso_from_unix(sub.get("current_period_end"))?,
            "cancelAtPeriodEnd": sub.get("cancel_at_period_end").cloned().unwrap_or(Value::Null),
            "tier": self.tier_of(price_id),
        }))
    }

    fn create_payment_intent(
        &self,
        params: &Map<String, Value>,
    ) -> Result<Value, IntegrationError> {
        let mut pairs = Pairs::default();
        flatten(&mut pairs, "amount", params.get("amount"));
        flatten(&mut pairs, "currency", params.get("currency"));
        flatten(&mut pairs, "metadata", params.get("metadata"));
        self.api("POST", "/v1/payment_intents", &pairs)
    }

    fn confirm_payment(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = encode_path_segment(text(params, "paymentIntentId"));
        self.api(
            "POST",
            &format!("/v1/payment_intents/{id}/confirm"),
            &Pairs::default(),
        )
    }

    fn refund(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let mut pairs = Pairs::default();
        flatten(&mut pairs, "payment_intent", params.get("paymentIntentId"));
        flatten(&mut pairs, "amount", params.get("amount"));
        self.api("POST", "/v1/refunds", &pairs)
    }

    fn create_customer(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let mut pairs = Pairs::default();
        pairs.push("email", text(params, "email"));
        if let Some(Value::String(name)) = params.get("displayName") {
            pairs.push("name", name);
        }
        pairs.push("metadata[almadarUid]", text(params, "almadarUid"));
        let customer = self.api("POST", "/v1/customers", &pairs)?;
        let echo = |value: Option<&Value>| match value {
            Some(Value::String(s)) => json!(s),
            _ => json!(""),
        };
        Ok(json!({
            "customerId": customer.get("id").cloned().unwrap_or(Value::Null),
            "email": echo(customer.get("email")),
            "almadarUid": echo(customer.get("metadata").and_then(|m| m.get("almadarUid"))),
        }))
    }

    fn create_checkout_session(
        &self,
        params: &Map<String, Value>,
    ) -> Result<Value, IntegrationError> {
        let customer = Some(text(params, "customerId")).filter(|c| !c.is_empty());
        let tier = tier_param(params.get("tier"))?;
        let quantity = js_number(params.get("quantity"));
        let metadata = string_metadata(params.get("metadata"))?;
        let price = self.price_for(tier)?;
        let mut pairs = Pairs::default();
        pairs.push("mode", "subscription");
        if let Some(customer) = customer {
            pairs.push("customer", customer);
        }
        pairs.push("line_items[0][price]", price);
        pairs.push("line_items[0][quantity]", &form::number_text(quantity));
        pairs.push("success_url", text(params, "successUrl"));
        pairs.push("cancel_url", text(params, "cancelUrl"));
        for (key, value) in &metadata {
            pairs.push(
                &format!("subscription_data[metadata][{}]", encode_component(key)),
                value,
            );
        }
        pairs.push("automatic_tax[enabled]", "true");
        pairs.push("allow_promotion_codes", "true");
        let session = self.api("POST", "/v1/checkout/sessions", &pairs)?;
        let customer_id = match session.get("customer") {
            Some(Value::String(id)) => id.clone(),
            Some(Value::Object(o)) => o
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => String::new(),
        };
        Ok(json!({
            "url": match session.get("url") { Some(Value::String(u)) => u.clone(), _ => String::new() },
            "sessionId": session.get("id").cloned().unwrap_or(Value::Null),
            "customerId": customer_id,
        }))
    }

    fn create_portal_session(
        &self,
        params: &Map<String, Value>,
    ) -> Result<Value, IntegrationError> {
        let customer = text(params, "customerId");
        let mut pairs = Pairs::default();
        pairs.push("customer", customer);
        pairs.push("return_url", text(params, "returnUrl"));
        let session = self.api("POST", "/v1/billing_portal/sessions", &pairs)?;
        Ok(json!({
            "url": session.get("url").cloned().unwrap_or(Value::Null),
            "customerId": customer,
        }))
    }

    fn get_subscription(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let sub = self.retrieve_subscription(text(params, "subscriptionId"))?;
        self.shape_subscription(&sub)
    }

    fn retrieve_subscription(&self, id: &str) -> Result<Value, IntegrationError> {
        self.api(
            "GET",
            &format!("/v1/subscriptions/{}", encode_path_segment(id)),
            &Pairs::default(),
        )
    }

    fn create_subscription(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let tier = tier_param(params.get("tier"))?;
        let quantity = js_number(params.get("quantity"));
        let metadata = string_metadata(params.get("metadata"))?;
        let price = self.price_for(tier)?;
        let mut pairs = Pairs::default();
        pairs.push("customer", text(params, "customerId"));
        pairs.push("items[0][price]", price);
        pairs.push("items[0][quantity]", &form::number_text(quantity));
        for (key, value) in &metadata {
            pairs.push(&format!("metadata[{}]", encode_component(key)), value);
        }
        pairs.push("proration_behavior", "create_prorations");
        pairs.push("automatic_tax[enabled]", "true");
        let sub = self.api("POST", "/v1/subscriptions", &pairs)?;
        self.shape_subscription(&sub)
    }

    fn update_subscription(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = text(params, "subscriptionId");
        let mut pairs = Pairs::default();
        pairs.push("proration_behavior", "create_prorations");
        if let Some(Value::Number(quantity)) = params.get("quantity") {
            let existing = self.retrieve_subscription(id)?;
            let item_id = existing
                .get("items")
                .and_then(|i| i.get("data"))
                .and_then(Value::as_array)
                .and_then(|d| d.first())
                .and_then(|i| i.get("id"))
                .and_then(Value::as_str);
            if let Some(item_id) = item_id {
                pairs.push("items[0][id]", item_id);
                pairs.push(
                    "items[0][quantity]",
                    &form::number_text(quantity.as_f64().unwrap_or(f64::NAN)),
                );
            }
        }
        if let Some(Value::Bool(flag)) = params.get("cancelAtPeriodEnd") {
            pairs.push("cancel_at_period_end", if *flag { "true" } else { "false" });
        }
        let sub = self.api(
            "POST",
            &format!("/v1/subscriptions/{}", encode_path_segment(id)),
            &pairs,
        )?;
        self.shape_subscription(&sub)
    }

    fn cancel_subscription(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = encode_path_segment(text(params, "subscriptionId"));
        let at_period_end = params.get("atPeriodEnd") != Some(&Value::Bool(false));
        let sub = if at_period_end {
            let mut pairs = Pairs::default();
            pairs.push("cancel_at_period_end", "true");
            self.api("POST", &format!("/v1/subscriptions/{id}"), &pairs)?
        } else {
            self.api(
                "DELETE",
                &format!("/v1/subscriptions/{id}"),
                &Pairs::default(),
            )?
        };
        self.shape_subscription(&sub)
    }
}

fn text<'a>(params: &'a Map<String, Value>, name: &str) -> &'a str {
    params.get(name).and_then(Value::as_str).unwrap_or_default()
}

fn tier_param(value: Option<&Value>) -> Result<&'static str, IntegrationError> {
    match value.and_then(Value::as_str) {
        Some("solo") => Ok("solo"),
        Some("teams") => Ok("teams"),
        _ => Err(IntegrationError::plain(
            "tier must be \"solo\" or \"teams\"",
        )),
    }
}

fn string_metadata(value: Option<&Value>) -> Result<Vec<(String, String)>, IntegrationError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(key, v)| match v {
                Value::String(s) => Ok((key.clone(), s.clone())),
                _ => Err(IntegrationError::plain(format!(
                    "metadata.{key} must be a string"
                ))),
            })
            .collect(),
        Some(_) => Err(IntegrationError::plain(
            "metadata must be an object of strings",
        )),
    }
}

fn connection_message(retries: usize) -> String {
    let retried = if retries > 0 {
        format!(" Request was retried {retries} times.")
    } else {
        String::new()
    };
    format!("An error occurred with our connection to Stripe.{retried}")
}

fn should_retry(response: &HttpResponse, retries: usize) -> bool {
    if retries >= MAX_NETWORK_RETRIES {
        return false;
    }
    match response.header("stripe-should-retry") {
        Some("false") => false,
        Some("true") => true,
        _ => response.status == 409 || response.status >= 500,
    }
}

/// `_jsonResponseHandler`: a body with a truthy `error` field fails with its message (any status); otherwise
/// the body is the result (any status).
fn read_response(response: &HttpResponse) -> Result<Value, IntegrationError> {
    let body: Value = serde_json::from_str(&response.text())
        .map_err(|_| IntegrationError::plain("Invalid JSON received from the Stripe API"))?;
    let error = body.get("error").filter(|e| match e {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    });
    if let Some(error) = error {
        let message = match error {
            Value::String(_) => body.get("error_description"),
            other => other.get("message"),
        };
        let message = match message {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        return Err(IntegrationError::plain(message));
    }
    Ok(body)
}

impl Provider for StripeProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "createPaymentIntent" => self.create_payment_intent(params),
            "confirmPayment" => self.confirm_payment(params),
            "refund" => self.refund(params),
            "createCustomer" => self.create_customer(params),
            "createCheckoutSession" => self.create_checkout_session(params),
            "createPortalSession" => self.create_portal_session(params),
            "getSubscription" => self.get_subscription(params),
            "createSubscription" => self.create_subscription(params),
            "updateSubscription" => self.update_subscription(params),
            "cancelSubscription" => self.cancel_subscription(params),
            other => Err(IntegrationError::plain(format!(
                "Unknown {SERVICE} action: {other}"
            ))),
        }
    }
}
