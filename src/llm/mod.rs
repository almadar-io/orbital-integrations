//! The `llm` service: the Rust twin of `LLMIntegration` (`@almadar/integrations` `integrations/llm`) over
//! `@almadar/llm`. Actions: `generate`, `classify`, `classifyMany`, `extract`, `summarize`, `embed`.
//! `call-tools` needs the host ports the kernel lends and is not ported yet.

mod chat;
mod config;
mod embed;
mod extract;
mod zod;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};

use self::chat::{build_request, parse_reply, ChatSettings, Reply};
pub use self::config::{ChatProvider, CONFIGURING_VARS};
use self::extract::parse_json_response;
use crate::credentials::Credentials;
use crate::error::{IntegrationError, SERVICE_ERROR};
use crate::provider::{CallContext, Integrations, Provider};
use crate::transport::{Transport, TransportError};

pub const SERVICE: &str = "llm";

const DEFAULT_TEMPERATURE: f64 = 0.7;
const DEFAULT_GENERATE_MAX_TOKENS: u64 = 1024;
const CALL_MAX_RETRIES: usize = 2;
const RAW_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const LOCAL_RAW_TIMEOUT: Duration = Duration::from_secs(20 * 60);

type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

pub struct LlmProvider {
    creds: Credentials,
    provider: ChatProvider,
    transport: Arc<dyn Transport>,
    sleeper: Sleeper,
}

/// Whether the credentials configure the `llm` service at all (`RuntimeIntegrationManager`).
pub fn is_configured(creds: &Credentials) -> bool {
    CONFIGURING_VARS.iter().any(|var| creds.get(var).is_some())
}

/// Register `llm` on `integrations`: ready when configured, failing with the TS constructor's error when the
/// credentials name no chat provider, unconfigured otherwise.
pub fn register(
    integrations: &mut Integrations,
    creds: &Credentials,
    transport: Arc<dyn Transport>,
) {
    if !is_configured(creds) {
        integrations.register_unconfigured(SERVICE);
        return;
    }
    match LlmProvider::new(creds, transport) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(error) => integrations.register_failed(SERVICE, error),
    }
}

impl LlmProvider {
    pub fn new(
        creds: &Credentials,
        transport: Arc<dyn Transport>,
    ) -> Result<Self, IntegrationError> {
        let provider = config::select_provider(creds)?;
        Ok(Self {
            creds: creds.clone(),
            provider,
            transport,
            sleeper: Arc::new(std::thread::sleep),
        })
    }

    /// Replace the wait between embedding retries (tests; hosts with their own clock).
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    pub fn chat_provider(&self) -> ChatProvider {
        self.provider
    }

    fn settings(
        &self,
        model: Option<&str>,
        temperature: f64,
    ) -> Result<ChatSettings, IntegrationError> {
        let endpoint = config::endpoint(self.provider, &self.creds)?;
        let model = model
            .filter(|m| !m.is_empty())
            .map_or_else(|| endpoint.default_model.clone(), str::to_string);
        Ok(ChatSettings {
            provider: self.provider,
            endpoint,
            model,
            temperature,
        })
    }

    fn invoke(
        &self,
        settings: &ChatSettings,
        system: &str,
        user: &str,
        max_tokens: Option<u64>,
        timeout: Option<Duration>,
    ) -> Result<Reply, IntegrationError> {
        let user = if settings.model.contains("qwen3.5") {
            format!("/no_think\n{user}")
        } else {
            user.to_string()
        };
        let request = build_request(settings, system, &user, max_tokens, timeout);
        let response = self.transport.send(&request).map_err(|e| match e {
            TransportError::TimedOut => IntegrationError::plain(format!(
                "LLM call timed out after {}ms",
                timeout.map_or(0, |t| t.as_millis())
            )),
            TransportError::Failed(detail) => {
                IntegrationError::plain(format!("Connection error: {detail}"))
            }
        })?;
        parse_reply(settings, &response)
    }

    /// `LLMClient.callWithMetadata`: ask for JSON, retry with the failure quoted, never retry a rate limit.
    fn call_json<T>(
        &self,
        settings: &ChatSettings,
        system: &str,
        user: &str,
        read: &dyn Fn(&Value) -> Result<T, String>,
    ) -> Result<T, IntegrationError> {
        let mut prompt = user.to_string();
        let mut last = IntegrationError::plain("unreachable");
        for attempt in 0..=CALL_MAX_RETRIES {
            let outcome = self
                .invoke(settings, system, &prompt, None, None)
                .and_then(|reply| {
                    parse_json_response(&reply.content).map_err(IntegrationError::plain)
                })
                .and_then(|value| read(&value).map_err(IntegrationError::plain));
            match outcome {
                Ok(done) => return Ok(done),
                Err(error) => {
                    let lowered = error.message.to_lowercase();
                    if lowered.contains("rate limit")
                        || lowered.contains("429")
                        || lowered.contains("quota exceeded")
                    {
                        return Err(error);
                    }
                    last = error;
                    if attempt < CALL_MAX_RETRIES {
                        prompt = format!("{user}\n\n[Previous attempt failed with: {}]\nPlease output valid JSON that matches the expected schema.", last.message);
                    }
                }
            }
        }
        Err(last)
    }

    fn generate(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let system = opt_str(params, "systemPrompt")
            .filter(|s| !s.is_empty())
            .unwrap_or("You are a helpful assistant.");
        let user = req_str(params, "userPrompt")?;
        let temperature = opt_f64(params, "temperature").unwrap_or(DEFAULT_TEMPERATURE);
        let settings = self.settings(opt_str(params, "model"), temperature)?;
        let max_tokens =
            opt_f64(params, "maxTokens").map_or(DEFAULT_GENERATE_MAX_TOKENS, |n| n as u64);
        let timeout = if self.provider == ChatProvider::Lmstudio {
            LOCAL_RAW_TIMEOUT
        } else {
            RAW_TIMEOUT
        };
        let reply = self.invoke(&settings, system, user, Some(max_tokens), Some(timeout))?;
        Ok(json!({
            "content": reply.content,
            "usage": {
                "promptTokens": reply.input_tokens,
                "completionTokens": reply.output_tokens,
                "totalTokens": reply.input_tokens + reply.output_tokens,
            },
        }))
    }

    fn classify(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let text = req_str(params, "text")?;
        let categories = str_list(params, "categories")?;
        let settings = self.settings(opt_str(params, "model"), DEFAULT_TEMPERATURE)?;
        let settings = ChatSettings {
            temperature: 0.1,
            ..settings
        };
        let system = format!(
            "You are a text classifier. Classify the given text into one of these categories: {}. Return JSON with: category, confidence (0-1), and reasoning.",
            categories.join(", ")
        );
        self.call_json(&settings, &system, text, &|value| {
            zod::classification(value, &categories)
                .map_err(|i| format!("Schema validation failed: {}", zod::format_issues(&i)))
        })
    }

    fn extract(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let text = req_str(params, "text")?;
        let schema = params.get("schema").cloned().unwrap_or(Value::Null);
        let settings = ChatSettings {
            temperature: 0.2,
            ..self.settings(opt_str(params, "model"), DEFAULT_TEMPERATURE)?
        };
        let description = serde_json::to_string_pretty(&schema).unwrap_or_default();
        let system = format!(
            "You are a structured data extractor. Extract data from the given text according to this schema:\n\n{description}\n\nReturn ONLY valid JSON matching the schema."
        );
        self.call_json(&settings, &system, text, &|value| Ok(value.clone()))
    }

    fn summarize(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let text = req_str(params, "text")?;
        let settings = ChatSettings {
            temperature: 0.3,
            ..self.settings(opt_str(params, "model"), DEFAULT_TEMPERATURE)?
        };
        let style = match opt_str(params, "style") {
            Some("bullet") => "Use bullet points.",
            Some("detailed") => "Be thorough and detailed.",
            _ => "Be concise.",
        };
        let length = match opt_f64(params, "maxLength") {
            Some(n) if n != 0.0 => format!("Keep the summary under {n} words."),
            _ => String::new(),
        };
        let system = format!("You are a text summarizer. {style} {length} Return JSON with: summary (string) and keyPoints (array of strings).");
        self.call_json(&settings, &system, text, &|value| {
            zod::summary(value)
                .map_err(|i| format!("Schema validation failed: {}", zod::format_issues(&i)))
        })
    }

    fn classify_many(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let items = id_text_items(params, "items")?;
        let categories = str_list(params, "categories")?;
        if let Some(model) = params.get("model") {
            if !model.is_string() {
                return Err(invalid("model", "a string"));
            }
        }
        if categories.is_empty() {
            return Err(IntegrationError::validation(
                "Param 'categories' must name at least one category",
            ));
        }
        if items.is_empty() {
            return Ok(json!({ "verdicts": [] }));
        }
        let settings = ChatSettings {
            temperature: 0.1,
            ..self.settings(opt_str(params, "model"), DEFAULT_TEMPERATURE)?
        };
        let instructions = [
            "Classify each item below into exactly one of these categories:".to_string(),
            categories.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n"),
            "Answer with one verdict per item: its id, the category, and your confidence from 0 to 1.".to_string(),
        ]
        .join("\n");
        let system = format!("You are a text classifier. {instructions} Return JSON: {{ \"verdicts\": [{{ \"id\", \"category\", \"confidence\" }}] }}.");
        let listing = items
            .iter()
            .enumerate()
            .map(|(i, (_, text))| format!("[{}] {text}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let verdicts = self
            .call_json(&settings, &system, &listing, &|value| {
                zod::verdicts(value, &categories)
                    .map_err(|i| format!("Schema validation failed: {}", zod::format_issues(&i)))
            })
            .map_err(|e| {
                IntegrationError::coded(
                    SERVICE_ERROR,
                    format!(
                        "The model's reply could not be read as verdicts: {}",
                        e.message
                    ),
                )
            })?;
        check_verdicts(&items, &categories, &verdicts)
    }

    fn embed(&self, params: &Map<String, Value>) -> Result<Value, IntegrationError> {
        let texts: Vec<String> = match params.get("texts") {
            Some(Value::Array(items)) if items.iter().all(Value::is_string) => items
                .iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect(),
            _ => {
                return Err(IntegrationError::plain(
                    "llm.embed: `texts` must be an array of strings",
                ))
            }
        };
        let sleeper = self.sleeper.clone();
        let wait = move |d: Duration| sleeper(d);
        embed::embed(&self.creds, &self.transport, &wait, &texts)
    }
}

impl Provider for LlmProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        match action {
            "generate" => self.generate(params),
            "classify" => self.classify(params),
            "classifyMany" => self.classify_many(params),
            "extract" => self.extract(params),
            "summarize" => self.summarize(params),
            "embed" => self.embed(params),
            "call-tools" => Err(IntegrationError::plain(
                "llm.call-tools is not available in the Rust provider yet: it needs the host ports the kernel lends",
            )),
            other => Err(IntegrationError::plain(format!("Unknown llm action: {other}"))),
        }
    }
}

fn invalid(name: &str, expected: &str) -> IntegrationError {
    IntegrationError::validation(format!("Param '{name}' must be {expected}"))
}

fn opt_str<'a>(params: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Value::as_str)
}

fn req_str<'a>(params: &'a Map<String, Value>, name: &str) -> Result<&'a str, IntegrationError> {
    opt_str(params, name).ok_or_else(|| invalid(name, "a string"))
}

fn opt_f64(params: &Map<String, Value>, name: &str) -> Option<f64> {
    params.get(name).and_then(Value::as_f64)
}

fn str_list(params: &Map<String, Value>, name: &str) -> Result<Vec<String>, IntegrationError> {
    let Some(Value::Array(items)) = params.get(name) else {
        return Err(invalid(name, "a list of strings"));
    };
    items
        .iter()
        .map(|i| {
            i.as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid(name, "a list of strings"))
        })
        .collect()
}

fn id_text_items(
    params: &Map<String, Value>,
    name: &str,
) -> Result<Vec<(String, String)>, IntegrationError> {
    let Some(Value::Array(raw)) = params.get(name) else {
        return Err(invalid(name, "a list of { id, text }"));
    };
    let mut items = Vec::new();
    for (i, item) in raw.iter().enumerate() {
        let Some(map) = item.as_object() else {
            return Err(invalid(&format!("{name}[{i}]"), "an object { id, text }"));
        };
        items.push((
            req_str(map, "id")?.to_string(),
            req_str(map, "text")?.to_string(),
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    for (id, _) in &items {
        if seen.contains(&id.as_str()) {
            return Err(invalid(
                name,
                &format!("a list with distinct ids; '{id}' appears twice"),
            ));
        }
        seen.push(id);
    }
    Ok(items)
}

/// `checkVerdicts`: one verdict per item, by the label the model saw, under the caller's id.
fn check_verdicts(
    items: &[(String, String)],
    categories: &[String],
    verdicts: &[(String, String, Value)],
) -> Result<Value, IntegrationError> {
    let fail = |message: String| IntegrationError::coded(SERVICE_ERROR, message);
    let mut by_label: Vec<(usize, Value)> = Vec::new();
    for (label, category, confidence) in verdicts {
        let Some(index) = (0..items.len()).find(|i| (i + 1).to_string() == *label) else {
            return Err(fail(format!(
                "The model gave a verdict for item '{label}', which was not sent"
            )));
        };
        if by_label.iter().any(|(i, _)| *i == index) {
            return Err(fail(format!("The model gave item '{label}' two verdicts")));
        }
        if !categories.contains(category) {
            return Err(fail(format!(
                "The model chose '{category}' for item '{label}', not a declared category"
            )));
        }
        by_label.push((
            index,
            json!({ "id": items[index].0, "category": category, "confidence": confidence }),
        ));
    }
    let missing: Vec<&str> = items
        .iter()
        .enumerate()
        .filter(|(i, _)| !by_label.iter().any(|(j, _)| j == i))
        .map(|(_, (id, _))| id.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(fail(format!(
            "The model gave no verdict for {}",
            missing.join(", ")
        )));
    }
    by_label.sort_by_key(|(i, _)| *i);
    Ok(json!({ "verdicts": by_label.into_iter().map(|(_, v)| v).collect::<Vec<_>>() }))
}
