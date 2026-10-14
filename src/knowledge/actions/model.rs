//! The model port over OpenAI-compatible chat completions: the twin of what `engine/server/llm.ts` asks of
//! `@almadar/llm`'s `LLMClient` for the three providers the knowledge actions use (`deepseek`, `openrouter`,
//! `openai`). Requests and replies are JSON:
//!
//! - `call`: `{ systemPrompt, userPrompt, provider, model, temperature?, maxTokens?, disableReasoning? }` ->
//!   `{ content, raw, model }` (`callRaw`).
//! - `stream`: the same request; the reply body is read whole and replayed as chunks, so progress reaches the
//!   caller after the model finishes, not while it writes (a `Transport` holds no open stream).
//! - `generate_structured`: the same request plus `schema` (see [`schema`]) -> the validated value
//!   (`LLMClient.call`: up to three attempts, each retry telling the model what failed).

use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::knowledge::store::{KnowledgeError, ModelChunk, ModelPort, ModelStream, StoreResult};
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};

const DEFAULT_TEMPERATURE: f64 = 0.3;
const RAW_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_RETRIES: usize = 2;

/// The JSON shape language a structured request validates its reply against (the zod schemas of the TS actions).
pub mod schema {
    use serde_json::{json, Value};

    pub fn string(min1: bool, trim: bool) -> Value {
        json!({ "t": "str", "min1": min1, "trim": trim })
    }
    pub fn number() -> Value {
        json!({ "t": "num" })
    }
    pub fn boolean() -> Value {
        json!({ "t": "bool" })
    }
    /// `z.number().int()`.
    pub fn integer() -> Value {
        json!({ "t": "num", "int": true })
    }
    /// `z.enum([...])`.
    pub fn one_of(values: &[&str]) -> Value {
        json!({ "t": "enum", "values": values })
    }
    pub fn array(of: Value, min: Option<usize>, max: Option<usize>) -> Value {
        json!({ "t": "arr", "of": of, "min": min, "max": max })
    }
    /// An object; names in `optional` may be absent.
    pub fn object(fields: Vec<(&str, Value)>, optional: &[&str]) -> Value {
        let fields: serde_json::Map<String, Value> = fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        json!({ "t": "obj", "fields": fields, "optional": optional })
    }
    pub fn nullable(of: Value) -> Value {
        json!({ "t": "nullable", "of": of })
    }
    /// A discriminated union on the string field `key`.
    pub fn union(key: &str, options: Vec<(&str, Value)>) -> Value {
        let options: serde_json::Map<String, Value> = options
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        json!({ "t": "union", "key": key, "options": options })
    }
}

fn received(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Validate `value` against a [`schema`]; returns the value with `trim` applied, or the issues zod would list.
pub fn check_shape(value: &Value, shape: &Value) -> Result<Value, Vec<String>> {
    let mut issues = Vec::new();
    let out = check(value, shape, &mut Vec::new(), &mut issues);
    if issues.is_empty() {
        Ok(out)
    } else {
        Err(issues)
    }
}

fn issue(path: &[String], message: String, issues: &mut Vec<String>) {
    issues.push(format!("{}: {message}", path.join(".")));
}

fn check(value: &Value, shape: &Value, path: &mut Vec<String>, issues: &mut Vec<String>) -> Value {
    match shape.get("t").and_then(Value::as_str).unwrap_or_default() {
        "str" => match value.as_str() {
            None => {
                issue(
                    path,
                    format!("Expected string, received {}", received(value)),
                    issues,
                );
                value.clone()
            }
            Some(s) => {
                let shown = if shape.get("trim").and_then(Value::as_bool) == Some(true) {
                    s.trim()
                } else {
                    s
                };
                if shape.get("min1").and_then(Value::as_bool) == Some(true) && shown.is_empty() {
                    issue(
                        path,
                        "String must contain at least 1 character(s)".to_string(),
                        issues,
                    );
                }
                Value::String(shown.to_string())
            }
        },
        "num" => {
            if !value.is_number() {
                issue(
                    path,
                    format!("Expected number, received {}", received(value)),
                    issues,
                );
            } else if shape.get("int").and_then(Value::as_bool) == Some(true)
                && value.as_f64().is_some_and(|f| f.fract() != 0.0)
            {
                issue(path, "Expected integer, received float".to_string(), issues);
            }
            value.clone()
        }
        "enum" => {
            let values: Vec<&str> = shape
                .get("values")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let joined = values
                .iter()
                .map(|v| format!("'{v}'"))
                .collect::<Vec<_>>()
                .join(" | ");
            match value.as_str() {
                Some(s) if values.contains(&s) => {}
                Some(s) => issue(
                    path,
                    format!("Invalid enum value. Expected {joined}, received '{s}'"),
                    issues,
                ),
                None => issue(
                    path,
                    format!("Expected {joined}, received {}", received(value)),
                    issues,
                ),
            }
            value.clone()
        }
        "bool" => {
            if !value.is_boolean() {
                issue(
                    path,
                    format!("Expected boolean, received {}", received(value)),
                    issues,
                );
            }
            value.clone()
        }
        "record" => {
            if !value.is_object() {
                issue(
                    path,
                    format!("Expected object, received {}", received(value)),
                    issues,
                );
            }
            value.clone()
        }
        "nullable" => {
            if value.is_null() {
                Value::Null
            } else {
                check(value, &shape["of"], path, issues)
            }
        }
        "arr" => match value.as_array() {
            None => {
                issue(
                    path,
                    format!("Expected array, received {}", received(value)),
                    issues,
                );
                value.clone()
            }
            Some(items) => {
                if let Some(min) = shape.get("min").and_then(Value::as_u64) {
                    if (items.len() as u64) < min {
                        issue(
                            path,
                            format!("Array must contain at least {min} element(s)"),
                            issues,
                        );
                    }
                }
                if let Some(max) = shape.get("max").and_then(Value::as_u64) {
                    if (items.len() as u64) > max {
                        issue(
                            path,
                            format!("Array must contain at most {max} element(s)"),
                            issues,
                        );
                    }
                }
                let mut out = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    path.push(i.to_string());
                    out.push(check(item, &shape["of"], path, issues));
                    path.pop();
                }
                Value::Array(out)
            }
        },
        "obj" => match value.as_object() {
            None => {
                issue(
                    path,
                    format!("Expected object, received {}", received(value)),
                    issues,
                );
                value.clone()
            }
            Some(map) => {
                let optional: Vec<&str> = shape
                    .get("optional")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let mut out = Map::new();
                for (name, field_shape) in shape
                    .get("fields")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    path.push(name.clone());
                    match map.get(name) {
                        None if optional.contains(&name.as_str()) => {}
                        None => issue(path, "Required".to_string(), issues),
                        Some(v) => {
                            out.insert(name.clone(), check(v, field_shape, path, issues));
                        }
                    }
                    path.pop();
                }
                Value::Object(out)
            }
        },
        "union" => {
            let key = shape.get("key").and_then(Value::as_str).unwrap_or_default();
            let options = shape.get("options").and_then(Value::as_object);
            let chosen = value
                .get(key)
                .and_then(Value::as_str)
                .and_then(|k| options.and_then(|o| o.get(k)));
            match chosen {
                Some(option) => check(value, option, path, issues),
                None => {
                    let expected = options
                        .map(|o| {
                            o.keys()
                                .map(|k| format!("'{k}'"))
                                .collect::<Vec<_>>()
                                .join(" | ")
                        })
                        .unwrap_or_default();
                    path.push(key.to_string());
                    issue(
                        path,
                        format!("Invalid discriminator value. Expected {expected}"),
                        issues,
                    );
                    path.pop();
                    value.clone()
                }
            }
        }
        _ => value.clone(),
    }
}

fn balanced(text: &str, start: usize, open: char, close: char) -> Option<String> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, ch) in text[start..].char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(text[start..start + i + ch.len_utf8()].to_string());
            }
        }
    }
    None
}

/// `extractJsonFromText`: the JSON a model reply carries, possibly in a code block or around prose.
pub fn extract_json_from_text(text: &str) -> Option<String> {
    let think = Regex::new(r"(?s)<think>.*?</think>\s*").ok()?;
    let stripped = think.replace_all(text, "");
    let trimmed = stripped.trim();
    if let Some(caps) = Regex::new(r"(?s)```(?:json)?\s*(.*?)```")
        .ok()?
        .captures(trimmed)
    {
        return Some(caps[1].trim().to_string());
    }
    let object_start = trimmed.find('{');
    let array_start = trimmed.find('[');
    let object_first =
        object_start.is_some() && (array_start.is_none() || object_start < array_start);
    let array_first =
        array_start.is_some() && (object_start.is_none() || array_start < object_start);
    if array_first {
        let start = array_start?;
        if let Some(json) = balanced(trimmed, start, '[', ']') {
            return Some(json);
        }
        if let Some(m) = Regex::new(r"(?s)\[.*\]").ok()?.find(trimmed) {
            return Some(m.as_str().to_string());
        }
    }
    if object_first {
        let start = object_start?;
        if let Some(json) = balanced(trimmed, start, '{', '}') {
            return Some(json);
        }
        if let Some(m) = Regex::new(r"(?s)\{.*\}").ok()?.find(trimmed) {
            return Some(m.as_str().to_string());
        }
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || Regex::new(r"^-?\d+(\.\d+)?([eE][+-]?\d+)?$")
            .ok()?
            .is_match(trimmed)
        || matches!(trimmed, "true" | "false" | "null")
    {
        return Some(trimmed.to_string());
    }
    None
}

fn fix_common_json_issues(json: &str) -> String {
    let mut fixed = json.to_string();
    if let Ok(re) = Regex::new(r",(\s*[}\]])") {
        fixed = re.replace_all(&fixed, "$1").into_owned();
    }
    if let Ok(re) = Regex::new(r"([{,]\s*)(\w+)(\s*:)") {
        fixed = re.replace_all(&fixed, "$1\"$2\"$3").into_owned();
    }
    fixed = fixed.replace('\'', "\"");
    fixed
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// `parseJsonResponse`: the JSON in a reply, checked against `shape` when given.
pub fn parse_json_response(response: &str, shape: Option<&Value>) -> Result<Value, String> {
    let Some(json_str) = extract_json_from_text(response) else {
        return Err("No valid JSON found in response. Expected a JSON value (object, array, string, number, boolean, or null), possibly wrapped in markdown code blocks.".to_string());
    };
    let parsed: Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(first) => serde_json::from_str(&fix_common_json_issues(&json_str)).map_err(|_| {
            let raw: String = json_str.chars().take(200).collect();
            format!("Failed to parse JSON: {first}. Raw text: {raw}...")
        })?,
    };
    match shape {
        Some(shape) => check_shape(&parsed, shape)
            .map_err(|issues| format!("Schema validation failed: {}", issues.join("; "))),
        None => Ok(parsed),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Provider {
    Openai,
    Deepseek,
    Openrouter,
}

struct Endpoint {
    provider: Provider,
    base: String,
    api_key: String,
    model: String,
}

struct Ask<'a> {
    system: &'a str,
    user: &'a str,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
    reasoning_effort_none: bool,
}

/// OpenAI-compatible chat completions over a `Transport`, configured from the same credentials as `@almadar/llm`.
pub struct OpenAiCompatModels {
    creds: Credentials,
    transport: Arc<dyn Transport>,
}

fn uses_max_completion_tokens(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("o1") || m.starts_with("gpt-5") || m.contains("o1-") || m.contains("o3")
}

fn truthy_message(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::String(_) | Value::Null | Value::Bool(false) => None,
        other => Some(other.to_string()),
    }
}

/// The OpenAI SDK's `APIError.makeMessage`, with the LangChain troubleshooting suffix it adds for a few statuses.
fn http_error(response: &HttpResponse) -> KnowledgeError {
    let parsed: Option<Value> = serde_json::from_str(&response.text())
        .ok()
        .filter(|v: &Value| !v.is_null());
    let msg: Option<String> = match &parsed {
        Some(body) => match body.get("error") {
            Some(error) if !error.is_null() => Some(
                error
                    .get("message")
                    .and_then(truthy_message)
                    .unwrap_or_else(|| error.to_string()),
            ),
            _ => None,
        },
        None => Some(response.text().into_owned()).filter(|t| !t.is_empty()),
    };
    let message = match msg {
        Some(m) => format!("{} {m}", response.status),
        None => format!("{} status code (no body)", response.status),
    };
    let code = match response.status {
        401 => Some("MODEL_AUTHENTICATION"),
        404 => Some("MODEL_NOT_FOUND"),
        429 => Some("MODEL_RATE_LIMIT"),
        _ => None,
    };
    KnowledgeError::other(match code {
        Some(code) => format!("{message}\n\nTroubleshooting URL: https://docs.langchain.com/oss/javascript/langchain/errors/{code}/\n"),
        None => message,
    })
}

impl OpenAiCompatModels {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        Self {
            creds: creds.clone(),
            transport,
        }
    }

    fn endpoint(&self, request: &Value) -> StoreResult<Endpoint> {
        let (provider, key_var, base, default_model) = match request
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("deepseek")
        {
            "openai" => (
                Provider::Openai,
                "OPENAI_API_KEY",
                self.creds
                    .get("OPENAI_BASE_URL")
                    .map_or_else(|| "https://api.openai.com/v1".to_string(), str::to_string),
                "gpt-4o",
            ),
            "openrouter" => (
                Provider::Openrouter,
                "OPEN_ROUTER_API_KEY",
                "https://openrouter.ai/api/v1".to_string(),
                "qwen/qwen-2.5-72b-instruct",
            ),
            _ => (
                Provider::Deepseek,
                "DEEPSEEK_API_KEY",
                "https://api.deepseek.com/v1".to_string(),
                "deepseek-v4-flash",
            ),
        };
        let Some(api_key) = self.creds.get(key_var) else {
            return Err(KnowledgeError::other(format!(
                "{key_var} environment variable is not set. Please set it in your .env file or environment."
            )));
        };
        let model = request
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .unwrap_or(default_model);
        Ok(Endpoint {
            provider,
            base,
            api_key: api_key.to_string(),
            model: model.to_string(),
        })
    }

    fn request_body(&self, endpoint: &Endpoint, ask: &Ask<'_>, stream: bool) -> Value {
        let completion_tokens = uses_max_completion_tokens(&endpoint.model);
        let mut body = Map::new();
        body.insert("model".into(), json!(endpoint.model));
        if !completion_tokens {
            body.insert(
                "temperature".into(),
                json!(ask.temperature.unwrap_or(DEFAULT_TEMPERATURE)),
            );
        }
        body.insert("stream".into(), json!(stream));
        if stream {
            body.insert("stream_options".into(), json!({ "include_usage": true }));
        }
        if ask.reasoning_effort_none {
            body.insert("reasoning_effort".into(), json!("none"));
        }
        if endpoint.provider == Provider::Openrouter {
            body.insert("tool_choice".into(), json!("auto"));
            body.insert("usage".into(), json!({ "include": true }));
            if ask.reasoning_effort_none {
                body.insert("reasoning".into(), json!({ "enabled": false }));
            }
        }
        if let Some(max) = ask.max_tokens.filter(|m| *m > 0) {
            body.insert(
                if completion_tokens {
                    "max_completion_tokens"
                } else {
                    "max_tokens"
                }
                .into(),
                json!(max),
            );
        }
        let user = if endpoint.model.contains("qwen3.5") {
            format!("/no_think\n{}", ask.user)
        } else {
            ask.user.to_string()
        };
        body.insert(
            "messages".into(),
            json!([{ "role": "system", "content": ask.system }, { "role": "user", "content": user }]),
        );
        Value::Object(body)
    }

    fn send(
        &self,
        endpoint: &Endpoint,
        body: &Value,
        timeout: Duration,
    ) -> StoreResult<HttpResponse> {
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{}/chat/completions", endpoint.base),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "application/json".to_string()),
                (
                    "authorization".to_string(),
                    format!("Bearer {}", endpoint.api_key),
                ),
            ],
            body: body.to_string().into_bytes(),
            timeout: Some(timeout),
        };
        let response = self.transport.send(&request).map_err(|e| match e {
            TransportError::TimedOut => KnowledgeError::other(format!(
                "LLM call timed out after {}ms",
                timeout.as_millis()
            )),
            TransportError::Failed(reason) => {
                KnowledgeError::other(format!("Connection error. {reason}"))
            }
        })?;
        if response.is_success() {
            Ok(response)
        } else {
            Err(http_error(&response))
        }
    }

    fn content_of(response: &HttpResponse) -> StoreResult<String> {
        let body: Value = serde_json::from_str(&response.text())
            .map_err(|e| KnowledgeError::other(format!("The model's reply is unreadable: {e}")))?;
        Ok(
            match body
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
            {
                Some(Value::String(s)) => s.clone(),
                None | Some(Value::Null) => String::new(),
                Some(other) => other.to_string(),
            },
        )
    }

    fn ask<'a>(request: &'a Value) -> Ask<'a> {
        Ask {
            system: request
                .get("systemPrompt")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            user: request
                .get("userPrompt")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            max_tokens: request.get("maxTokens").and_then(Value::as_u64),
            temperature: request.get("temperature").and_then(Value::as_f64),
            reasoning_effort_none: request.get("disableReasoning").and_then(Value::as_bool)
                == Some(true),
        }
    }
}

fn is_rate_limit(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("rate limit") || m.contains("429") || m.contains("quota exceeded")
}

fn sse_chunks(body: &str) -> Vec<ModelChunk> {
    let mut chunks = Vec::new();
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let content = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !content.is_empty() {
            chunks.push(ModelChunk {
                content: content.to_string(),
                done: false,
            });
        }
    }
    chunks.push(ModelChunk {
        content: String::new(),
        done: true,
    });
    chunks
}

impl ModelPort for OpenAiCompatModels {
    fn stream(&self, request: &Value) -> StoreResult<ModelStream> {
        let endpoint = self.endpoint(request)?;
        let ask = Self::ask(request);
        let body = self.request_body(&endpoint, &ask, true);
        // `LLMClient.streamRaw` is lazy: an HTTP failure surfaces on the first read, after the caller announced the model
        let response = match self.send(&endpoint, &body, RAW_TIMEOUT) {
            Ok(response) => response,
            Err(error) => {
                return Ok(ModelStream {
                    model: endpoint.model,
                    chunks: Box::new(std::iter::once(Err(error))),
                })
            }
        };
        let chunks = sse_chunks(&response.text());
        Ok(ModelStream {
            model: endpoint.model,
            chunks: Box::new(chunks.into_iter().map(Ok)),
        })
    }

    fn call(&self, request: &Value) -> StoreResult<Value> {
        let endpoint = self.endpoint(request)?;
        let ask = Self::ask(request);
        let body = self.request_body(&endpoint, &ask, false);
        let response = self.send(&endpoint, &body, RAW_TIMEOUT)?;
        let content = Self::content_of(&response)?;
        Ok(json!({ "content": content, "raw": content, "model": endpoint.model }))
    }

    fn generate_structured(&self, request: &Value) -> StoreResult<Value> {
        let endpoint = self.endpoint(request)?;
        let ask = Self::ask(request);
        let shape = request.get("schema");
        let mut prompt = ask.user.to_string();
        let mut last = KnowledgeError::other("The model made no attempt");
        for attempt in 0..=MAX_RETRIES {
            let attempt_ask = Ask {
                user: &prompt,
                ..Self::ask(request)
            };
            let body = self.request_body(&endpoint, &attempt_ask, false);
            let outcome = self
                .send(&endpoint, &body, CALL_TIMEOUT)
                .and_then(|response| Self::content_of(&response))
                .and_then(|content| {
                    parse_json_response(&content, shape).map_err(KnowledgeError::other)
                });
            match outcome {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if is_rate_limit(&error.message) {
                        return Err(error);
                    }
                    if attempt < MAX_RETRIES {
                        prompt = format!(
                            "{}\n\n[Previous attempt failed with: {}]\nPlease output valid JSON that matches the expected schema.",
                            ask.user, error.message
                        );
                    }
                    last = error;
                }
            }
        }
        Err(last)
    }
}

// ---- embeddings ----

const EMBED_MAX_ATTEMPTS: usize = 3;
const EMBED_BACKOFF_MS: [u64; 2] = [500, 2000];
const EMBED_RATE_LIMIT_MAX_ATTEMPTS: usize = 4;
const EMBED_RATE_LIMIT_BACKOFF_MS: [u64; 3] = [2000, 5000, 15000];

/// Embeddings for the knowledge features (`getEmbeddingClient`): OpenRouter's bge when `OPEN_ROUTER_API_KEY` is set,
/// else OpenAI's small model when `OPENAI_API_KEY` is; no key means no embeddings.
/// A failed embedding request: its message, HTTP status and `Retry-After`.
type EmbedFailure = (String, Option<u16>, Option<Duration>);

pub struct HttpEmbedder {
    creds: Credentials,
    transport: Arc<dyn Transport>,
    sleeper: Arc<dyn Fn(Duration) + Send + Sync>,
}

impl HttpEmbedder {
    pub fn new(creds: &Credentials, transport: Arc<dyn Transport>) -> Self {
        Self {
            creds: creds.clone(),
            transport,
            sleeper: Arc::new(std::thread::sleep),
        }
    }

    pub fn with_sleeper(mut self, sleeper: Arc<dyn Fn(Duration) + Send + Sync>) -> Self {
        self.sleeper = sleeper;
        self
    }

    fn client(&self) -> Option<(&'static str, String, &'static str, String)> {
        if let Some(key) = self.creds.get("OPEN_ROUTER_API_KEY") {
            return Some((
                "openrouter",
                "https://openrouter.ai/api/v1".to_string(),
                "baai/bge-base-en-v1.5",
                key.to_string(),
            ));
        }
        self.creds.get("OPENAI_API_KEY").map(|key| {
            (
                "openai",
                "https://api.openai.com/v1".to_string(),
                "text-embedding-3-small",
                key.to_string(),
            )
        })
    }

    fn once(
        &self,
        name: &str,
        base: &str,
        model: &str,
        key: &str,
        texts: &[String],
    ) -> Result<Vec<Vec<f64>>, EmbedFailure> {
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{base}/embeddings"),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "*/*".to_string()),
                ("authorization".to_string(), format!("Bearer {key}")),
            ],
            body: json!({ "model": model, "input": texts })
                .to_string()
                .into_bytes(),
            timeout: None,
        };
        let response = self
            .transport
            .send(&request)
            .map_err(|e| (format!("fetch failed: {e:?}"), None, None))?;
        if !response.is_success() {
            let retry_after = response
                .header("retry-after")
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err((
                format!(
                    "EmbeddingClient: {name} returned {} {}: {}",
                    response.status,
                    response.status_text(),
                    response.text()
                ),
                Some(response.status),
                retry_after,
            ));
        }
        let body: Value = serde_json::from_str(&response.text()).map_err(|_| {
            (
                format!(
                    "EmbeddingClient: {name} returned unparseable body: {}",
                    response.text()
                ),
                None,
                None,
            )
        })?;
        let Some(data) = body.get("data").and_then(Value::as_array) else {
            return Err((
                format!("EmbeddingClient: {name} returned no embeddings"),
                None,
                None,
            ));
        };
        let mut rows: Vec<(usize, Vec<f64>)> = data
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let index = d
                    .get("index")
                    .and_then(Value::as_u64)
                    .map_or(i, |n| n as usize);
                let vector = d
                    .get("embedding")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_f64).collect())
                    .unwrap_or_default();
                (index, vector)
            })
            .collect();
        rows.sort_by_key(|(i, _)| *i);
        if rows.len() != texts.len() {
            return Err((
                format!(
                    "EmbeddingClient: {name} returned {} embeddings for {} inputs",
                    rows.len(),
                    texts.len()
                ),
                None,
                None,
            ));
        }
        Ok(rows.into_iter().map(|(_, v)| v).collect())
    }
}

impl super::Embedder for HttpEmbedder {
    fn embed_batch(&self, texts: &[String]) -> Option<Vec<Vec<f64>>> {
        let (name, base, model, key) = self.client()?;
        if texts.is_empty() {
            return None;
        }
        let (mut generic, mut limited) = (0usize, 0usize);
        loop {
            match self.once(name, &base, model, &key, texts) {
                Ok(vectors) => return Some(vectors),
                Err((message, status, retry_after)) => {
                    let lowered = message.to_lowercase();
                    if [
                        "context length",
                        "maximum input length",
                        "reduce the length of the input",
                        "input_tokens",
                    ]
                    .iter()
                    .any(|n| lowered.contains(n))
                    {
                        return None;
                    }
                    if status == Some(429) {
                        if limited >= EMBED_RATE_LIMIT_MAX_ATTEMPTS - 1 {
                            return None;
                        }
                        (self.sleeper)(retry_after.unwrap_or(Duration::from_millis(
                            EMBED_RATE_LIMIT_BACKOFF_MS[limited],
                        )));
                        limited += 1;
                        continue;
                    }
                    generic += 1;
                    if generic >= EMBED_MAX_ATTEMPTS {
                        return None;
                    }
                    (self.sleeper)(Duration::from_millis(EMBED_BACKOFF_MS[generic - 1]));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_found_in_code_blocks_prose_and_think_tags() {
        assert_eq!(
            extract_json_from_text("```json\n{\"a\":1}\n```").as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            extract_json_from_text("Sure! {\"a\":{\"b\":2}} done").as_deref(),
            Some("{\"a\":{\"b\":2}}")
        );
        assert_eq!(
            extract_json_from_text("<think>hm {x}</think>\n[1,2]").as_deref(),
            Some("[1,2]")
        );
        assert_eq!(extract_json_from_text("nothing here"), None);
        assert_eq!(extract_json_from_text("42").as_deref(), Some("42"));
    }

    #[test]
    fn common_json_slips_are_repaired_before_parsing() {
        let parsed = parse_json_response("{name: 'x', list: [1, 2,],}", None).unwrap();
        assert_eq!(parsed, json!({ "name": "x", "list": [1, 2] }));
    }

    #[test]
    fn a_reply_is_checked_against_the_shape_and_trimmed() {
        let shape = schema::object(
            vec![(
                "labels",
                schema::array(schema::string(true, true), Some(1), Some(5)),
            )],
            &[],
        );
        assert_eq!(
            parse_json_response("{\"labels\":[\" a \"]}", Some(&shape)).unwrap(),
            json!({ "labels": ["a"] })
        );
        let err = parse_json_response("{\"labels\":[]}", Some(&shape)).unwrap_err();
        assert_eq!(
            err,
            "Schema validation failed: labels: Array must contain at least 1 element(s)"
        );
        let err = parse_json_response("{}", Some(&shape)).unwrap_err();
        assert_eq!(err, "Schema validation failed: labels: Required");
    }

    #[test]
    fn a_discriminated_union_picks_its_option() {
        let shape = schema::union(
            "kind",
            vec![(
                "a",
                schema::object(
                    vec![
                        ("kind", schema::string(false, false)),
                        ("x", schema::number()),
                    ],
                    &[],
                ),
            )],
        );
        assert!(check_shape(&json!({ "kind": "a", "x": 1 }), &shape).is_ok());
        assert_eq!(
            check_shape(&json!({ "kind": "b" }), &shape).unwrap_err()[0],
            "kind: Invalid discriminator value. Expected 'a'"
        );
    }

    #[test]
    fn http_failures_read_like_the_sdk_with_the_langchain_suffix_for_auth() {
        let bad = HttpResponse {
            status: 400,
            headers: Vec::new(),
            body: "{\"error\":{\"message\":\"Invalid request\"}}".into(),
        };
        assert_eq!(http_error(&bad).message, "400 Invalid request");
        let auth = HttpResponse {
            status: 401,
            headers: Vec::new(),
            body: "{\"error\":{\"message\":\"bad key\"}}".into(),
        };
        assert!(http_error(&auth)
            .message
            .ends_with("errors/MODEL_AUTHENTICATION/\n"));
    }

    struct Script {
        replies: std::sync::Mutex<std::collections::VecDeque<HttpResponse>>,
        sent: std::sync::Mutex<Vec<HttpRequest>>,
    }

    impl Transport for Script {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
            self.sent.lock().unwrap().push(request.clone());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TransportError::Failed("script exhausted".into()))
        }
    }

    fn script(replies: Vec<HttpResponse>) -> Arc<Script> {
        Arc::new(Script {
            replies: std::sync::Mutex::new(replies.into()),
            sent: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn reply(status: u16, body: Value) -> HttpResponse {
        HttpResponse {
            status,
            headers: Vec::new(),
            body: body.to_string().into_bytes(),
        }
    }

    fn creds(pairs: &[(&str, &str)]) -> Credentials {
        Credentials::from_env(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn embeddings_come_back_in_input_order_from_the_keyed_provider() {
        let t = script(vec![reply(
            200,
            json!({ "data": [{ "index": 1, "embedding": [2.0] }, { "index": 0, "embedding": [1.0] }] }),
        )]);
        let embedder = HttpEmbedder::new(&creds(&[("OPEN_ROUTER_API_KEY", "k")]), t.clone());
        let out =
            super::super::Embedder::embed_batch(&embedder, &["a".to_string(), "b".to_string()])
                .unwrap();
        assert_eq!(out, vec![vec![1.0], vec![2.0]]);
        let sent = t.sent.lock().unwrap();
        assert_eq!(sent[0].url, "https://openrouter.ai/api/v1/embeddings");
        assert_eq!(
            serde_json::from_str::<Value>(&sent[0].text()).unwrap(),
            json!({ "model": "baai/bge-base-en-v1.5", "input": ["a", "b"] })
        );
    }

    #[test]
    fn without_a_key_there_are_no_embeddings_and_no_request() {
        let t = script(vec![]);
        let embedder = HttpEmbedder::new(&creds(&[]), t.clone());
        assert!(super::super::Embedder::embed_batch(&embedder, &["a".to_string()]).is_none());
        assert!(t.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_rate_limit_waits_and_a_transient_failure_retries_with_backoff_then_gives_up() {
        let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = waits.clone();
        let t = script(vec![
            reply(429, json!({})),
            reply(500, json!({})),
            reply(500, json!({})),
            reply(500, json!({})),
        ]);
        let embedder = HttpEmbedder::new(&creds(&[("OPENAI_API_KEY", "k")]), t.clone())
            .with_sleeper(Arc::new(move |d| sink.lock().unwrap().push(d)));
        assert!(super::super::Embedder::embed_batch(&embedder, &["a".to_string()]).is_none());
        assert_eq!(
            *waits.lock().unwrap(),
            vec![
                Duration::from_millis(2000),
                Duration::from_millis(500),
                Duration::from_millis(2000)
            ]
        );
        assert_eq!(t.sent.lock().unwrap().len(), 4);
        assert_eq!(
            t.sent.lock().unwrap()[0].url,
            "https://api.openai.com/v1/embeddings"
        );
    }

    #[test]
    fn a_structured_call_retries_with_what_failed_and_stops_at_a_rate_limit() {
        let bad = || {
            reply(
                200,
                json!({ "choices": [{ "message": { "content": "{\"labels\":[]}" } }] }),
            )
        };
        let t = script(vec![
            bad(),
            bad(),
            reply(
                200,
                json!({ "choices": [{ "message": { "content": "{\"labels\":[\"a\"]}" } }] }),
            ),
        ]);
        let models = OpenAiCompatModels::new(&creds(&[("DEEPSEEK_API_KEY", "k")]), t.clone());
        let shape = schema::object(
            vec![(
                "labels",
                schema::array(schema::string(true, false), Some(1), None),
            )],
            &[],
        );
        let out = models
            .generate_structured(
                &json!({ "systemPrompt": "s", "userPrompt": "u", "schema": shape }),
            )
            .unwrap();
        assert_eq!(out, json!({ "labels": ["a"] }));
        let second: Value = serde_json::from_str(&t.sent.lock().unwrap()[1].text()).unwrap();
        assert!(second["messages"][1]["content"].as_str().unwrap().contains("[Previous attempt failed with: Schema validation failed: labels: Array must contain at least 1 element(s)]"));

        let limited = script(vec![reply(
            429,
            json!({ "error": { "message": "slow down" } }),
        )]);
        let models = OpenAiCompatModels::new(&creds(&[("DEEPSEEK_API_KEY", "k")]), limited.clone());
        assert!(models
            .generate_structured(&json!({ "systemPrompt": "s", "userPrompt": "u" }))
            .is_err());
        assert_eq!(limited.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_stream_replays_the_event_stream_as_chunks_and_a_failure_surfaces_on_the_first_read() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\ndata: [DONE]\n\n";
        let t = script(vec![
            HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: sse.into(),
            },
            reply(400, json!({ "error": { "message": "no" } })),
        ]);
        let models = OpenAiCompatModels::new(&creds(&[("DEEPSEEK_API_KEY", "k")]), t);
        let stream = models
            .stream(&json!({ "systemPrompt": "s", "userPrompt": "u" }))
            .unwrap();
        let chunks: Vec<_> = stream.chunks.map(|c| c.unwrap()).collect();
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.content.as_str())
                .collect::<Vec<_>>(),
            ["He", "llo", ""]
        );
        assert!(chunks.last().unwrap().done);
        let failing = models
            .stream(&json!({ "systemPrompt": "s", "userPrompt": "u" }))
            .unwrap();
        assert_eq!(
            failing.chunks.last().unwrap().unwrap_err().message,
            "400 no"
        );
    }
}
