//! The `otel` service: the Rust twin of `OtelIntegration` (`@almadar/integrations` `integrations/otel`), an
//! in-memory span and metric recorder. Actions: `startSpan`, `endSpan`, `addEvent`, `recordMetric`, `getSpan`, `getMetrics`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::infra::{
    configured_from_declared, opt_f64, opt_str, random_hex_id, str_param, unknown_action, Rand,
    SystemRand,
};
use crate::json::number;
use crate::provider::{CallContext, Integrations, Provider};
use crate::wallclock::{system_clock, Clock};

pub const SERVICE: &str = "otel";

struct Event {
    name: String,
    timestamp: i64,
    attributes: Option<Value>,
}

struct Span {
    span_id: String,
    trace_id: String,
    name: String,
    start_time: i64,
    end_time: Option<i64>,
    status: Option<String>,
    attributes: Value,
    events: Vec<Event>,
}

impl Span {
    fn view(&self) -> Value {
        let mut out = Map::new();
        out.insert("spanId".into(), json!(self.span_id));
        out.insert("traceId".into(), json!(self.trace_id));
        out.insert("name".into(), json!(self.name));
        out.insert("startTime".into(), json!(self.start_time));
        if let Some(end) = self.end_time {
            out.insert("endTime".into(), json!(end));
        }
        if let Some(status) = &self.status {
            out.insert("status".into(), json!(status));
        }
        out.insert("attributes".into(), self.attributes.clone());
        let events: Vec<Value> = self
            .events
            .iter()
            .map(|event| {
                let mut e = Map::new();
                e.insert("name".into(), json!(event.name));
                e.insert("timestamp".into(), json!(event.timestamp));
                if let Some(attributes) = &event.attributes {
                    e.insert("attributes".into(), attributes.clone());
                }
                Value::Object(e)
            })
            .collect();
        out.insert("events".into(), Value::Array(events));
        Value::Object(out)
    }
}

struct Metric {
    name: String,
    kind: String,
    value: f64,
    values: Vec<f64>,
    labels: Value,
}

impl Metric {
    fn view(&self) -> Value {
        json!({
            "name": self.name,
            "type": self.kind,
            "value": number(self.value),
            "values": self.values.iter().map(|v| number(*v)).collect::<Vec<_>>(),
            "labels": self.labels,
        })
    }
}

#[derive(Default)]
struct State {
    spans: HashMap<String, Span>,
    metrics: HashMap<String, Metric>,
}

pub struct OtelProvider {
    state: Mutex<State>,
    clock: Clock,
    rand: Arc<dyn Rand>,
}

/// Whether the credentials configure `otel` (`OTEL_EXPORTER_OTLP_ENDPOINT` is a required declared credential).
pub fn is_configured(creds: &Credentials) -> bool {
    configured_from_declared(creds, SERVICE)
}

pub fn register(integrations: &mut Integrations, creds: &Credentials) {
    if is_configured(creds) {
        integrations.register(SERVICE, Arc::new(OtelProvider::new()));
    } else {
        integrations.register_unconfigured(SERVICE);
    }
}

impl Default for OtelProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OtelProvider {
    pub fn new() -> Self {
        Self::with_sources(system_clock(), Arc::new(SystemRand::new()))
    }

    pub fn with_sources(clock: Clock, rand: Arc<dyn Rand>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            clock,
            rand,
        }
    }
}

impl Provider for OtelProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        let mut state = self.state.lock().expect("otel state");
        let now = (self.clock)();
        match action {
            "startSpan" => {
                let name = str_param(params, "name")?;
                let attributes = params
                    .get("attributes")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let span_id = random_hex_id(&*self.rand, 8);
                let trace_id = match opt_str(params, "traceId") {
                    Some(id) => id.to_string(),
                    None => random_hex_id(&*self.rand, 16),
                };
                state.spans.insert(
                    span_id.clone(),
                    Span {
                        span_id: span_id.clone(),
                        trace_id: trace_id.clone(),
                        name: name.to_string(),
                        start_time: now,
                        end_time: None,
                        status: None,
                        attributes,
                        events: Vec::new(),
                    },
                );
                Ok(json!({ "spanId": span_id, "traceId": trace_id }))
            }
            "endSpan" => {
                let span_id = str_param(params, "spanId")?;
                let Some(span) = state.spans.get_mut(span_id) else {
                    return Ok(json!({ "ended": false }));
                };
                span.end_time = Some(now);
                if let Some(status) = opt_str(params, "status").filter(|s| !s.is_empty()) {
                    span.status = Some(status.to_string());
                }
                Ok(json!({ "ended": true }))
            }
            "addEvent" => {
                let span_id = str_param(params, "spanId")?;
                let name = str_param(params, "name")?;
                let Some(span) = state.spans.get_mut(span_id) else {
                    return Ok(json!({ "added": false }));
                };
                span.events.push(Event {
                    name: name.to_string(),
                    timestamp: now,
                    attributes: params.get("attributes").cloned(),
                });
                Ok(json!({ "added": true }))
            }
            "recordMetric" => {
                let name = str_param(params, "name")?;
                let value = opt_f64(params, "value").unwrap_or(0.0);
                let kind = opt_str(params, "type").unwrap_or("counter");
                let labels = params.get("labels").cloned().unwrap_or_else(|| json!({}));
                match state.metrics.get_mut(name) {
                    Some(existing) => {
                        match kind {
                            "counter" => existing.value += value,
                            "gauge" => existing.value = value,
                            "histogram" => existing.values.push(value),
                            _ => {}
                        }
                        existing.labels = labels;
                    }
                    None => {
                        let histogram = kind == "histogram";
                        state.metrics.insert(
                            name.to_string(),
                            Metric {
                                name: name.to_string(),
                                kind: kind.to_string(),
                                value: if histogram { 0.0 } else { value },
                                values: if histogram { vec![value] } else { Vec::new() },
                                labels,
                            },
                        );
                    }
                }
                Ok(json!({ "recorded": true }))
            }
            "getSpan" => {
                let span_id = str_param(params, "spanId")?;
                Ok(state.spans.get(span_id).map_or(Value::Null, Span::view))
            }
            "getMetrics" => Ok(Value::Object(
                state
                    .metrics
                    .iter()
                    .map(|(key, metric)| (key.clone(), metric.view()))
                    .collect(),
            )),
            other => Err(unknown_action("otel", other)),
        }
    }
}
