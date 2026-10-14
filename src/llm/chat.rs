//! One chat completion over the wire: the request `@langchain/openai` / `@langchain/anthropic` send, and how
//! their replies and failures read back. Recorded exchanges in `fixtures/llm/` pin each shape.

use std::time::Duration;

use serde_json::{json, Map, Value};

use super::config::{ChatProvider, Endpoint};
use crate::error::IntegrationError;
use crate::transport::{HttpRequest, HttpResponse};

const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 8192;
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct ChatSettings {
    pub provider: ChatProvider,
    pub endpoint: Endpoint,
    pub model: String,
    pub temperature: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub content: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// `LLMClient.usesMaxCompletionTokens`.
fn client_treats_as_reasoning(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("o1") || m.starts_with("gpt-5") || m.contains("o1-") || m.contains("o3")
}

/// `isReasoningModel` in `@langchain/openai`: an `o<digit>` model, or `gpt-5` other than `gpt-5-chat`.
fn langchain_treats_as_reasoning(model: &str) -> bool {
    let mut chars = model.chars();
    let o_digit = chars.next() == Some('o') && chars.next().is_some_and(|c| c.is_ascii_digit());
    o_digit || (model.starts_with("gpt-5") && !model.starts_with("gpt-5-chat"))
}

pub fn build_request(
    settings: &ChatSettings,
    system: &str,
    user: &str,
    max_tokens: Option<u64>,
    timeout: Option<Duration>,
) -> HttpRequest {
    let max_tokens = max_tokens.filter(|n| *n > 0);
    let (url, headers, body) = if settings.provider.is_anthropic() {
        let body = json!({
            "model": settings.model,
            "stream": false,
            "max_tokens": max_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS),
            "temperature": crate::json::number(settings.temperature),
            "messages": [{ "role": "user", "content": user }],
            "system": [{ "type": "text", "text": system, "cache_control": { "type": "ephemeral" } }],
        });
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
            ("x-api-key".to_string(), settings.endpoint.api_key.clone()),
            (
                "anthropic-version".to_string(),
                ANTHROPIC_VERSION.to_string(),
            ),
        ];
        (
            format!("{}/v1/messages", settings.endpoint.base_url),
            headers,
            body,
        )
    } else {
        let client_reasoning = client_treats_as_reasoning(&settings.model);
        let lc_reasoning = langchain_treats_as_reasoning(&settings.model);
        let mut body = Map::new();
        body.insert("model".into(), json!(settings.model));
        if !client_reasoning {
            let temperature = if settings.provider == ChatProvider::Kimi {
                0.6
            } else {
                settings.temperature
            };
            body.insert("temperature".into(), crate::json::number(temperature));
        }
        body.insert("stream".into(), json!(false));
        if settings.provider == ChatProvider::Kimi {
            body.insert("thinking".into(), json!({ "type": "disabled" }));
        }
        if settings.provider == ChatProvider::Openrouter {
            body.insert("tool_choice".into(), json!("auto"));
            body.insert("usage".into(), json!({ "include": true }));
        }
        let constructor_max = if client_reasoning { None } else { max_tokens };
        if lc_reasoning {
            if let Some(n) = constructor_max {
                body.insert("max_completion_tokens".into(), json!(n));
            }
        } else {
            if let Some(n) = constructor_max {
                body.insert("max_tokens".into(), json!(n));
            }
            if let (true, Some(n)) = (client_reasoning, max_tokens) {
                body.insert("max_completion_tokens".into(), json!(n));
            }
        }
        let system_role = if lc_reasoning { "developer" } else { "system" };
        body.insert("messages".into(), json!([{ "role": system_role, "content": system }, { "role": "user", "content": user }]));
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
            (
                "authorization".to_string(),
                format!("Bearer {}", settings.endpoint.api_key),
            ),
        ];
        (
            format!("{}/chat/completions", settings.endpoint.base_url),
            headers,
            Value::Object(body),
        )
    };
    HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body: body.to_string().into_bytes(),
        timeout,
    }
}

fn whole(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_f64).map_or(0, |n| n as i64)
}

pub fn parse_reply(
    settings: &ChatSettings,
    response: &HttpResponse,
) -> Result<Reply, IntegrationError> {
    if !response.is_success() {
        return Err(http_error(settings.provider, response));
    }
    let body: Value = serde_json::from_str(&response.text()).map_err(|e| {
        IntegrationError::plain(format!(
            "{} returned an unreadable body: {e}",
            settings.model
        ))
    })?;
    if settings.provider.is_anthropic() {
        let blocks = body
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let content = match blocks.as_slice() {
            [single] if single.get("type").and_then(Value::as_str) == Some("text") => single
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            _ => Value::Array(blocks).to_string(),
        };
        let usage = body.get("usage");
        let input = whole(usage.and_then(|u| u.get("input_tokens")))
            + whole(usage.and_then(|u| u.get("cache_creation_input_tokens")))
            + whole(usage.and_then(|u| u.get("cache_read_input_tokens")));
        return Ok(Reply {
            content,
            input_tokens: input,
            output_tokens: whole(usage.and_then(|u| u.get("output_tokens"))),
        });
    }
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| IntegrationError::plain("The model's reply had no choices"))?;
    let content = match choice.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(s)) => s.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    };
    let usage = body.get("usage");
    Ok(Reply {
        content,
        input_tokens: whole(usage.and_then(|u| u.get("prompt_tokens"))),
        output_tokens: whole(usage.and_then(|u| u.get("completion_tokens"))),
    })
}

/// `JSON.stringify` of an already-compact-or-pretty JSON text: drops whitespace outside strings.
pub fn minify(json_text: &str) -> String {
    let mut out = String::with_capacity(json_text.len());
    let mut in_string = false;
    let mut escaped = false;
    for ch in json_text.chars() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if !ch.is_whitespace() {
            out.push(ch);
        }
    }
    out
}

fn truthy_message(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::String(_) | Value::Null | Value::Bool(false) => None,
        other => Some(other.to_string()),
    }
}

/// The SDKs' `APIError.makeMessage`.
fn api_error_message(provider: ChatProvider, status: u16, text: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(text)
        .ok()
        .filter(|v: &Value| !v.is_null());
    let msg: Option<String> = match (&parsed, provider.is_anthropic()) {
        (Some(body), true) => Some(
            body.get("message")
                .and_then(truthy_message)
                .unwrap_or_else(|| minify(text)),
        ),
        (Some(body), false) => match body.get("error") {
            Some(error) if !error.is_null() => Some(
                error
                    .get("message")
                    .and_then(truthy_message)
                    .unwrap_or_else(|| error.to_string()),
            ),
            _ => None,
        },
        (None, _) => Some(text.to_string()).filter(|t| !t.is_empty()),
    };
    match msg {
        Some(m) => format!("{status} {m}"),
        None => format!("{status} status code (no body)"),
    }
}

fn langchain_suffix(code: &str) -> String {
    format!("\n\nTroubleshooting URL: https://docs.langchain.com/oss/javascript/langchain/errors/{code}/\n")
}

fn http_error(provider: ChatProvider, response: &HttpResponse) -> IntegrationError {
    let message = api_error_message(provider, response.status, &response.text());
    let code = match (provider.is_anthropic(), response.status) {
        (false, 400) if message.contains("tool_calls") => Some("INVALID_TOOL_RESULTS"),
        (true, 400) if message.contains("tool") => Some("INVALID_TOOL_RESULTS"),
        (_, 401) => Some("MODEL_AUTHENTICATION"),
        (_, 404) => Some("MODEL_NOT_FOUND"),
        (_, 429) => Some("MODEL_RATE_LIMIT"),
        _ => None,
    };
    IntegrationError::plain(match code {
        Some(code) => format!("{message}{}", langchain_suffix(code)),
        None => message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(provider: ChatProvider, model: &str, temperature: f64) -> ChatSettings {
        ChatSettings {
            provider,
            endpoint: Endpoint {
                api_key: "k".into(),
                base_url: "http://h/v1".into(),
                default_model: "d".into(),
            },
            model: model.into(),
            temperature,
        }
    }

    fn body(req: &HttpRequest) -> Value {
        serde_json::from_str(&req.text()).unwrap()
    }

    fn resp(status: u16, text: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![],
            body: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn reasoning_models_drop_temperature_and_rename_the_system_role() {
        let b = body(&build_request(
            &settings(ChatProvider::Openai, "gpt-5.1", 0.7),
            "s",
            "u",
            Some(32),
            None,
        ));
        assert!(b.get("temperature").is_none());
        assert_eq!(b["messages"][0]["role"], "developer");
        assert!(b.get("max_tokens").is_none());
    }

    #[test]
    fn control_a_plain_model_keeps_temperature_system_role_and_max_tokens() {
        let b = body(&build_request(
            &settings(ChatProvider::Openai, "gpt-4o", 0.2),
            "s",
            "u",
            Some(32),
            None,
        ));
        assert_eq!(b["temperature"], 0.2);
        assert_eq!(b["messages"][0]["role"], "system");
        assert_eq!(b["max_tokens"], 32);
    }

    #[test]
    fn an_o4_model_is_reasoning_for_langchain_but_not_for_the_client() {
        let b = body(&build_request(
            &settings(ChatProvider::Openai, "o4-mini", 0.7),
            "s",
            "u",
            Some(16),
            None,
        ));
        assert_eq!(b["temperature"], 0.7);
        assert_eq!(b["max_completion_tokens"], 16);
        assert_eq!(b["messages"][0]["role"], "developer");
    }

    #[test]
    fn a_zero_max_tokens_sends_none() {
        let b = body(&build_request(
            &settings(ChatProvider::Openai, "gpt-4o", 0.2),
            "s",
            "u",
            Some(0),
            None,
        ));
        assert!(b.get("max_tokens").is_none());
    }

    #[test]
    fn kimi_pins_temperature_and_disables_thinking() {
        let b = body(&build_request(
            &settings(ChatProvider::Kimi, "kimi-k2.5", 0.2),
            "s",
            "u",
            None,
            None,
        ));
        assert_eq!(b["temperature"], 0.6);
        assert_eq!(b["thinking"]["type"], "disabled");
    }

    #[test]
    fn anthropic_defaults_max_tokens_and_marks_the_system_prompt_cacheable() {
        let req = build_request(
            &settings(ChatProvider::Anthropic, "m", 0.1),
            "s",
            "u",
            None,
            None,
        );
        let b = body(&req);
        assert_eq!(b["max_tokens"], 8192);
        assert_eq!(b["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(req.url.ends_with("/v1/messages"));
    }

    #[test]
    fn openai_reply_reads_content_and_usage_and_a_missing_usage_is_zero() {
        let s = settings(ChatProvider::Openai, "m", 0.1);
        let ok = parse_reply(&s, &resp(200, r#"{"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#)).unwrap();
        assert_eq!(
            ok,
            Reply {
                content: "hi".into(),
                input_tokens: 3,
                output_tokens: 1
            }
        );
        let none = parse_reply(
            &s,
            &resp(200, r#"{"choices":[{"message":{"content":"hi"}}]}"#),
        )
        .unwrap();
        assert_eq!((none.input_tokens, none.output_tokens), (0, 0));
    }

    #[test]
    fn anthropic_input_tokens_include_the_cache_counts() {
        let s = settings(ChatProvider::Anthropic, "m", 0.1);
        let r = parse_reply(
            &s,
            &resp(200, r#"{"content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":3,"cache_creation_input_tokens":4,"cache_read_input_tokens":5,"output_tokens":1}}"#),
        )
        .unwrap();
        assert_eq!(
            (r.content.as_str(), r.input_tokens, r.output_tokens),
            ("hi", 12, 1)
        );
    }

    #[test]
    fn error_messages_follow_the_sdks() {
        let openai = parse_reply(
            &settings(ChatProvider::Openai, "m", 0.1),
            &resp(400, r#"{"error":{"message":"Invalid request"}}"#),
        )
        .unwrap_err();
        assert_eq!(openai.message, "400 Invalid request");
        let anthropic = parse_reply(
            &settings(ChatProvider::Anthropic, "m", 0.1),
            &resp(400, "{\"type\": \"error\", \"error\": {\"type\": \"x\", \"message\": \"max_tokens: required\"}}"),
        )
        .unwrap_err();
        assert_eq!(
            anthropic.message,
            r#"400 {"type":"error","error":{"type":"x","message":"max_tokens: required"}}"#
        );
        let empty =
            parse_reply(&settings(ChatProvider::Openai, "m", 0.1), &resp(500, "")).unwrap_err();
        assert_eq!(empty.message, "500 status code (no body)");
        let text = parse_reply(
            &settings(ChatProvider::Openai, "m", 0.1),
            &resp(502, "Bad gateway"),
        )
        .unwrap_err();
        assert_eq!(text.message, "502 Bad gateway");
    }

    #[test]
    fn auth_and_missing_model_failures_carry_the_langchain_troubleshooting_url() {
        let unauthorized = parse_reply(
            &settings(ChatProvider::Openai, "m", 0.1),
            &resp(401, r#"{"error":{"message":"bad key"}}"#),
        )
        .unwrap_err();
        assert_eq!(
            unauthorized.message,
            "401 bad key\n\nTroubleshooting URL: https://docs.langchain.com/oss/javascript/langchain/errors/MODEL_AUTHENTICATION/\n"
        );
        let control = parse_reply(
            &settings(ChatProvider::Openai, "m", 0.1),
            &resp(403, r#"{"error":{"message":"nope"}}"#),
        )
        .unwrap_err();
        assert_eq!(control.message, "403 nope");
    }

    #[test]
    fn minify_keeps_string_contents_intact() {
        assert_eq!(
            minify("{ \"a\": \"x y\" ,\n \"b\": [1, 2] }"),
            r#"{"a":"x y","b":[1,2]}"#
        );
    }
}
