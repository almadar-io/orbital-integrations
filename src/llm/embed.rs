//! Embeddings over the OpenAI-compatible `/embeddings` endpoint: the twin of `@almadar/llm`'s
//! `EmbeddingClient` as `@almadar/integrations` configures it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};

const DEFAULT_MODEL: &str = "baai/bge-base-en-v1.5";
const MAX_ATTEMPTS: usize = 3;
const BACKOFF_MS: [u64; 2] = [500, 2000];
const RATE_LIMIT_MAX_ATTEMPTS: usize = 4;
const RATE_LIMIT_BACKOFF_MS: [u64; 3] = [2000, 5000, 15000];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingProvider {
    Openai,
    Openrouter,
    Lmstudio,
}

impl EmbeddingProvider {
    fn name(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Openrouter => "openrouter",
            Self::Lmstudio => "lmstudio",
        }
    }
}

struct Failure {
    error: IntegrationError,
    status: Option<u16>,
    retry_after: Option<Duration>,
}

impl Failure {
    fn plain(message: String) -> Self {
        Self {
            error: IntegrationError::plain(message),
            status: None,
            retry_after: None,
        }
    }
}

fn overflow(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "context length",
        "maximum input length",
        "reduce the length of the input",
        "input_tokens",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

pub fn embed(
    creds: &Credentials,
    transport: &Arc<dyn Transport>,
    sleep: &dyn Fn(Duration),
    texts: &[String],
) -> Result<Value, IntegrationError> {
    let provider = match creds.get("EMBEDDING_PROVIDER") {
        None | Some("openrouter") => EmbeddingProvider::Openrouter,
        Some("openai") => EmbeddingProvider::Openai,
        Some("lmstudio") => EmbeddingProvider::Lmstudio,
        Some(other) => {
            return Err(IntegrationError::plain(format!(
                "Unknown embedding provider: {other}"
            )))
        }
    };
    let model = creds
        .get("EMBEDDING_MODEL")
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let base_url = match provider {
        EmbeddingProvider::Openai => "https://api.openai.com/v1".to_string(),
        EmbeddingProvider::Openrouter => "https://openrouter.ai/api/v1".to_string(),
        EmbeddingProvider::Lmstudio => creds
            .get("LMSTUDIO_URL")
            .unwrap_or("http://127.0.0.1:1234/v1")
            .trim_end_matches('/')
            .to_string(),
    };
    let api_key = match provider {
        EmbeddingProvider::Lmstudio => creds
            .get("EMBEDDING_API_KEY")
            .unwrap_or("not-needed")
            .to_string(),
        EmbeddingProvider::Openai | EmbeddingProvider::Openrouter => {
            let env_var = if provider == EmbeddingProvider::Openai {
                "OPENAI_API_KEY"
            } else {
                "OPEN_ROUTER_API_KEY"
            };
            creds.get("EMBEDDING_API_KEY").or_else(|| creds.get(env_var)).map(str::to_string).ok_or_else(|| {
                IntegrationError::plain(format!(
                    "EmbeddingClient: API key for provider '{}' not found. Set {env_var} in your environment or pass options.apiKey.",
                    provider.name()
                ))
            })?
        }
    };

    if texts.is_empty() {
        return Ok(json!({ "embeddings": [] }));
    }

    let once = || embed_once(provider, &base_url, &model, &api_key, transport, texts);
    let mut generic_attempt = 0usize;
    let mut rate_limit_attempt = 0usize;
    loop {
        let failure = match once() {
            Ok(embeddings) => return Ok(json!({ "embeddings": embeddings })),
            Err(f) => f,
        };
        if overflow(&failure.error.message) {
            return Err(failure.error);
        }
        if failure.status == Some(429) {
            if rate_limit_attempt >= RATE_LIMIT_MAX_ATTEMPTS - 1 {
                return Err(failure.error);
            }
            let wait = failure.retry_after.unwrap_or(Duration::from_millis(
                RATE_LIMIT_BACKOFF_MS[rate_limit_attempt],
            ));
            rate_limit_attempt += 1;
            sleep(wait);
            continue;
        }
        generic_attempt += 1;
        if generic_attempt >= MAX_ATTEMPTS {
            return Err(failure.error);
        }
        sleep(Duration::from_millis(BACKOFF_MS[generic_attempt - 1]));
    }
}

fn embed_once(
    provider: EmbeddingProvider,
    base_url: &str,
    model: &str,
    api_key: &str,
    transport: &Arc<dyn Transport>,
    texts: &[String],
) -> Result<Vec<Value>, Failure> {
    let request = HttpRequest {
        method: "POST".into(),
        url: format!("{base_url}/embeddings"),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("accept".into(), "*/*".into()),
            ("authorization".into(), format!("Bearer {api_key}")),
        ],
        body: json!({ "model": model, "input": texts })
            .to_string()
            .into_bytes(),
        timeout: None,
    };
    let response: HttpResponse = transport.send(&request).map_err(|e| {
        Failure::plain(match e {
            TransportError::TimedOut => "fetch failed: the request timed out".to_string(),
            TransportError::Failed(detail) => format!("fetch failed: {detail}"),
        })
    })?;
    if !response.is_success() {
        let retry_after = response
            .header("retry-after")
            .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        return Err(Failure {
            error: IntegrationError::plain(format!(
                "EmbeddingClient: {} returned {} {}: {}",
                provider.name(),
                response.status,
                response.status_text(),
                response.text()
            )),
            status: Some(response.status),
            retry_after,
        });
    }
    let body: Value = serde_json::from_str(&response.text()).map_err(|_| {
        Failure::plain(format!(
            "EmbeddingClient: {} returned unparseable body: {}",
            provider.name(),
            response.text()
        ))
    })?;
    let Some(data) = body.get("data").and_then(Value::as_array) else {
        let detail = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map_or_else(|| response.text().into_owned(), str::to_string);
        return Err(Failure::plain(format!(
            "EmbeddingClient: {} response has no `data` array: {detail}",
            provider.name()
        )));
    };
    let mut ordered: Vec<(i64, Value)> = data
        .iter()
        .map(|d| {
            (
                d.get("index").and_then(Value::as_i64).unwrap_or(0),
                d.get("embedding").cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    ordered.sort_by_key(|(index, _)| *index);
    if ordered.len() != texts.len() {
        return Err(Failure::plain(format!(
            "EmbeddingClient: provider returned {} embeddings for {} inputs",
            ordered.len(),
            texts.len()
        )));
    }
    Ok(ordered
        .into_iter()
        .map(|(_, embedding)| embedding)
        .collect())
}
