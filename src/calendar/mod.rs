//! The `calendar` service: the Rust twin of `CalendarIntegration` (Google Calendar through a service
//! account, optionally acting as a Workspace user). Actions: `listEvents`, `createEvent`, `updateEvent`,
//! `deleteEvent`, `freeBusy`, `watch`, `stopWatch`. Event times are ISO strings on the wire; a 10-character
//! value (`YYYY-MM-DD`) is an all-day `date`, anything longer a `dateTime`.

use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::formwire::{encode_path_segment, number_text, Pairs};
use crate::google::{Body, GoogleClient, JwtSource, Sleeper};
use crate::jswire::{iso_from_ms, js_number, parse_date_ms, system_clock, Clock};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::Transport;

pub const SERVICE: &str = "calendar";
const ROOT: &str = "https://www.googleapis.com/calendar/v3";
const SCOPE: &str = "https://www.googleapis.com/auth/calendar";

pub struct CalendarProvider {
    client: GoogleClient,
    default_calendar_id: String,
}

/// The credentials that configure `calendar` (`RuntimeIntegrationManager`).
pub fn is_configured(creds: &Credentials) -> bool {
    creds.get("GOOGLE_CALENDAR_SA_KEY").is_some()
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
    match CalendarProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl CalendarProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        Self::with_runtime(
            creds,
            transport,
            system_clock(),
            Arc::new(std::thread::sleep),
        )
    }

    /// Build with an injected clock (token timestamps and expiry) and sleeper (retry waits).
    pub fn with_runtime(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
        clock: Clock,
        sleeper: Sleeper,
    ) -> Result<Self, IntegrationError> {
        let Some(raw) = creds.get("GOOGLE_CALENDAR_SA_KEY") else {
            return Err(IntegrationError::plain(
                "GOOGLE_CALENDAR_SA_KEY not configured",
            ));
        };
        let source = JwtSource::from_key(raw, SCOPE, creds.get("GOOGLE_CALENDAR_SUBJECT"), clock)?;
        Ok(Self {
            client: GoogleClient::new(Arc::new(source), transport, sleeper),
            default_calendar_id: creds
                .get("GOOGLE_CALENDAR_ID")
                .unwrap_or("primary")
                .to_string(),
        })
    }

    fn calendar_id(&self, params: &Map<String, Value>) -> String {
        match params.get("calendarId").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => self.default_calendar_id.clone(),
        }
    }

    fn events_path(&self, params: &Map<String, Value>) -> String {
        format!(
            "{ROOT}/calendars/{}/events",
            encode_path_segment(&self.calendar_id(params))
        )
    }

    fn list_events(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let mut query = Pairs::default();
        match text(params, "syncToken").filter(|t| !t.is_empty()) {
            Some(token) => query.push("syncToken", token),
            None => {
                for name in ["timeMin", "timeMax"] {
                    if let Some(value) = text(params, name).filter(|t| !t.is_empty()) {
                        query.push(name, value);
                    }
                }
                query.push("singleEvents", "true");
                query.push("orderBy", "startTime");
            }
        }
        let max = match js_number(params.get("maxResults")) {
            n if n == 0.0 || n.is_nan() => 250.0,
            n => n,
        };
        query.push("maxResults", &number_text(max));
        let url = format!("{}?{}", self.events_path(params), query.encode());
        let reply = self.client.request("GET", &url, Body::None)?;
        let events: Vec<Value> = reply
            .data
            .get("items")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(shape_event).collect())
            .unwrap_or_default();
        Ok(json!({
            "events": events,
            "nextSyncToken": reply.data.get("nextSyncToken").filter(|t| !t.is_null()).cloned().unwrap_or(Value::Null),
        }))
    }

    fn create_event(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let start = text(params, "start").unwrap_or_default();
        let end = resolve_end(
            start,
            text(params, "end"),
            js_number(params.get("durationMinutes")),
        )?;
        let mut body = Map::new();
        body.insert(
            "summary".into(),
            params.get("summary").cloned().unwrap_or(Value::Null),
        );
        for name in ["description", "location"] {
            if let Some(value) = text(params, name).filter(|t| !t.is_empty()) {
                body.insert(name.into(), json!(value));
            }
        }
        body.insert("start".into(), event_time(start));
        body.insert("end".into(), event_time(&end));
        let reply = self.client.request(
            "POST",
            &self.events_path(params),
            Body::Json(Value::Object(body)),
        )?;
        Ok(json!({
            "id": or_empty(&reply.data, "id"),
            "status": or_empty(&reply.data, "status"),
            "htmlLink": or_empty(&reply.data, "htmlLink"),
        }))
    }

    fn update_event(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let mut body = Map::new();
        for name in ["summary", "description", "location"] {
            if let Some(Value::String(value)) = params.get(name) {
                body.insert(name.into(), json!(value));
            }
        }
        for name in ["start", "end"] {
            if let Some(value) = text(params, name).filter(|t| !t.is_empty()) {
                body.insert(name.into(), event_time(value));
            }
        }
        let url = format!(
            "{}/{}",
            self.events_path(params),
            encode_path_segment(text(params, "eventId").unwrap_or_default())
        );
        let reply = self
            .client
            .request("PATCH", &url, Body::Json(Value::Object(body)))?;
        Ok(json!({
            "id": or_empty(&reply.data, "id"),
            "status": or_empty(&reply.data, "status"),
        }))
    }

    fn delete_event(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = text(params, "eventId").unwrap_or_default();
        let url = format!("{}/{}", self.events_path(params), encode_path_segment(id));
        self.client.request("DELETE", &url, Body::None)?;
        Ok(json!({ "id": id, "deleted": true }))
    }

    fn free_busy(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let id = self.calendar_id(params);
        let body = json!({
            "timeMin": params.get("timeMin").cloned().unwrap_or(Value::Null),
            "timeMax": params.get("timeMax").cloned().unwrap_or(Value::Null),
            "items": [{ "id": id }],
        });
        let reply = self
            .client
            .request("POST", &format!("{ROOT}/freeBusy"), Body::Json(body))?;
        let busy: Vec<Value> = reply
            .data
            .get("calendars")
            .and_then(|c| c.get(&id))
            .and_then(|c| c.get("busy"))
            .and_then(Value::as_array)
            .map(|slots| {
                slots
                    .iter()
                    .map(|slot| json!({ "start": or_empty(slot, "start"), "end": or_empty(slot, "end") }))
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "busy": busy }))
    }

    fn watch(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let channel_id = params.get("channelId").cloned().unwrap_or(Value::Null);
        let mut body = Map::new();
        body.insert("id".into(), channel_id.clone());
        body.insert("type".into(), json!("web_hook"));
        body.insert(
            "address".into(),
            params.get("address").cloned().unwrap_or(Value::Null),
        );
        let ttl = js_number(params.get("ttlSeconds"));
        if ttl != 0.0 && !ttl.is_nan() {
            body.insert("params".into(), json!({ "ttl": number_text(ttl) }));
        }
        let url = format!("{}/watch", self.events_path(params));
        let reply = self
            .client
            .request("POST", &url, Body::Json(Value::Object(body)))?;
        Ok(json!({
            "channelId": reply.data.get("id").filter(|v| !v.is_null()).cloned().unwrap_or(channel_id),
            "resourceId": or_empty(&reply.data, "resourceId"),
            "expiration": or_empty(&reply.data, "expiration"),
        }))
    }

    fn stop_watch(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let body = json!({
            "id": params.get("channelId").cloned().unwrap_or(Value::Null),
            "resourceId": params.get("resourceId").cloned().unwrap_or(Value::Null),
        });
        self.client
            .request("POST", &format!("{ROOT}/channels/stop"), Body::Json(body))?;
        Ok(json!({ "stopped": true }))
    }
}

fn text<'a>(params: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Value::as_str)
}

/// `value[field] ?? ''`.
fn or_empty(value: &Value, field: &str) -> Value {
    value
        .get(field)
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!(""))
}

fn event_time(value: &str) -> Value {
    if value.chars().count() == 10 {
        json!({ "date": value })
    } else {
        json!({ "dateTime": value })
    }
}

fn from_event_time(time: Option<&Value>) -> Value {
    let pick = |field: &str| {
        time.and_then(|t| t.get(field))
            .filter(|v| !v.is_null())
            .cloned()
    };
    pick("dateTime")
        .or_else(|| pick("date"))
        .unwrap_or_else(|| json!(""))
}

fn shape_event(event: &Value) -> Value {
    json!({
        "id": or_empty(event, "id"),
        "summary": or_empty(event, "summary"),
        "description": or_empty(event, "description"),
        "location": or_empty(event, "location"),
        "start": from_event_time(event.get("start")),
        "end": from_event_time(event.get("end")),
        "status": or_empty(event, "status"),
        "updated": or_empty(event, "updated"),
    })
}

fn invalid_time() -> IntegrationError {
    IntegrationError::plain("Invalid time value")
}

fn resolve_end(
    start: &str,
    end: Option<&str>,
    duration_minutes: f64,
) -> Result<String, IntegrationError> {
    if let Some(end) = end.filter(|e| !e.is_empty()) {
        return Ok(end.to_string());
    }
    if start.chars().count() == 10 {
        let next = parse_date_ms(start).ok_or_else(invalid_time)? + 24 * 60 * 60_000;
        return Ok(iso_from_ms(next)[..10].to_string());
    }
    if duration_minutes > 0.0 {
        let ms =
            parse_date_ms(start).ok_or_else(invalid_time)? as f64 + duration_minutes * 60_000.0;
        return Ok(iso_from_ms(ms as i64));
    }
    Err(IntegrationError::plain(
        "createEvent requires `end` or a positive `durationMinutes`",
    ))
}

impl Provider for CalendarProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "listEvents" => self.list_events(params),
            "createEvent" => self.create_event(params),
            "updateEvent" => self.update_event(params),
            "deleteEvent" => self.delete_event(params),
            "freeBusy" => self.free_busy(params),
            "watch" => self.watch(params),
            "stopWatch" => self.stop_watch(params),
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
    fn an_all_day_start_without_an_end_ends_the_next_day() {
        assert_eq!(resolve_end("2026-02-28", None, 0.0).unwrap(), "2026-03-01");
        assert_eq!(
            resolve_end("2028-02-28", Some(""), 0.0).unwrap(),
            "2028-02-29"
        );
    }

    #[test]
    fn a_timed_start_needs_an_end_or_a_positive_duration() {
        assert_eq!(
            resolve_end("2026-03-01T09:00:00Z", None, 45.0).unwrap(),
            "2026-03-01T09:45:00.000Z"
        );
        assert!(resolve_end("2026-03-01T09:00:00Z", None, 0.0).is_err());
        assert!(resolve_end("2026-03-01T09:00:00Z", None, -5.0).is_err());
        assert_eq!(
            resolve_end("2026-03-01T09:00:00Z", Some("X"), 0.0).unwrap(),
            "X"
        );
    }

    #[test]
    fn ten_character_times_are_dates() {
        assert_eq!(event_time("2026-03-01"), json!({ "date": "2026-03-01" }));
        assert_eq!(
            event_time("2026-03-01T09:00:00Z"),
            json!({ "dateTime": "2026-03-01T09:00:00Z" })
        );
    }
}
