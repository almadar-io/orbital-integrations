//! Which chat provider, with which key, endpoint and default model: the twin of `PROVIDER_CONFIGS` and
//! `getAvailableProvider` in `@almadar/llm` `client.ts`.

use crate::credentials::Credentials;
use crate::error::IntegrationError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatProvider {
    Anthropic,
    Deepseek,
    Kimi,
    Openai,
    Openrouter,
    Orbgen,
    Masar,
    Lmstudio,
}

/// Hosted providers' API-key env vars, in auto-detection priority order.
pub const PROVIDER_API_KEY_ENV: [(ChatProvider, &str); 5] = [
    (ChatProvider::Anthropic, "ANTHROPIC_API_KEY"),
    (ChatProvider::Deepseek, "DEEPSEEK_API_KEY"),
    (ChatProvider::Kimi, "KIMI_API_KEY"),
    (ChatProvider::Openai, "OPENAI_API_KEY"),
    (ChatProvider::Openrouter, "OPEN_ROUTER_API_KEY"),
];

/// Env names whose presence configures the `llm` service (`RuntimeIntegrationManager.configureFromEnv`).
pub const CONFIGURING_VARS: [&str; 9] = [
    "ANTHROPIC_API_KEY",
    "DEEPSEEK_API_KEY",
    "KIMI_API_KEY",
    "OPENAI_API_KEY",
    "OPEN_ROUTER_API_KEY",
    "LLM_PROVIDER",
    "EMBEDDING_PROVIDER",
    "EMBEDDING_MODEL",
    "EMBEDDING_API_KEY",
];

impl ChatProvider {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "anthropic" => Self::Anthropic,
            "deepseek" => Self::Deepseek,
            "kimi" => Self::Kimi,
            "openai" => Self::Openai,
            "openrouter" => Self::Openrouter,
            "orbgen" => Self::Orbgen,
            "masar" => Self::Masar,
            "lmstudio" => Self::Lmstudio,
            _ => return None,
        })
    }

    pub fn is_anthropic(self) -> bool {
        self == Self::Anthropic
    }
}

/// The chosen provider: `LLM_PROVIDER` when set, else the first hosted key found.
pub fn select_provider(creds: &Credentials) -> Result<ChatProvider, IntegrationError> {
    if let Some(name) = creds.get("LLM_PROVIDER") {
        return ChatProvider::parse(name)
            .ok_or_else(|| IntegrationError::plain(format!("Unknown LLM provider: {name}")));
    }
    PROVIDER_API_KEY_ENV
        .iter()
        .find(|(_, var)| creds.get(var).is_some())
        .map(|(provider, _)| *provider)
        .ok_or_else(|| {
            let vars: Vec<&str> = PROVIDER_API_KEY_ENV.iter().map(|(_, v)| *v).collect();
            IntegrationError::plain(format!(
                "No LLM API key found. Please set {}.",
                vars.join(", ")
            ))
        })
}

/// A resolved provider endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub api_key: String,
    /// Chat base URL without a trailing slash (the Anthropic origin for Anthropic).
    pub base_url: String,
    pub default_model: String,
}

fn required_key(creds: &Credentials, var: &str) -> Result<String, IntegrationError> {
    creds
        .get(var)
        .map(str::to_string)
        .ok_or_else(|| IntegrationError::plain(format!("{var} environment variable is not set. Please set it in your .env file or environment.")))
}

fn trim_slash(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

pub fn endpoint(provider: ChatProvider, creds: &Credentials) -> Result<Endpoint, IntegrationError> {
    Ok(match provider {
        ChatProvider::Openai => Endpoint {
            api_key: required_key(creds, "OPENAI_API_KEY")?,
            base_url: trim_slash(
                creds
                    .get("OPENAI_BASE_URL")
                    .unwrap_or("https://api.openai.com/v1"),
            ),
            default_model: "gpt-4o".into(),
        },
        ChatProvider::Anthropic => Endpoint {
            api_key: required_key(creds, "ANTHROPIC_API_KEY")?,
            base_url: trim_slash(
                creds
                    .get("ANTHROPIC_BASE_URL")
                    .unwrap_or("https://api.anthropic.com"),
            ),
            default_model: "claude-sonnet-4-5-20250929".into(),
        },
        ChatProvider::Deepseek => Endpoint {
            api_key: required_key(creds, "DEEPSEEK_API_KEY")?,
            base_url: "https://api.deepseek.com/v1".into(),
            default_model: "deepseek-v4-flash".into(),
        },
        ChatProvider::Kimi => Endpoint {
            api_key: required_key(creds, "KIMI_API_KEY")?,
            base_url: "https://api.moonshot.ai/v1".into(),
            default_model: "kimi-k2.5".into(),
        },
        ChatProvider::Openrouter => Endpoint {
            api_key: required_key(creds, "OPEN_ROUTER_API_KEY")?,
            base_url: "https://openrouter.ai/api/v1".into(),
            default_model: "qwen/qwen-2.5-72b-instruct".into(),
        },
        ChatProvider::Orbgen => {
            let url = creds.get("ORBGEN_URL").ok_or_else(|| {
                IntegrationError::plain(
                    "ORBGEN_URL environment variable is not set. Set it to the OrbGen Cloud Run URL (e.g., https://orbgen-v2-xxx.run.app)",
                )
            })?;
            Endpoint {
                api_key: "not-needed".into(),
                base_url: format!("{url}/v1"),
                default_model: "orbgen-v2".into(),
            }
        }
        ChatProvider::Masar => {
            let url = creds.get("MASAR_SUBAGENT_URL").ok_or_else(|| {
                IntegrationError::plain(
                    "MASAR_SUBAGENT_URL environment variable is not set. Set it to the masar subagent endpoint (e.g., https://masar-subagent-xxx.run.app)",
                )
            })?;
            Endpoint {
                api_key: creds
                    .get("MASAR_SUBAGENT_API_KEY")
                    .unwrap_or("not-needed")
                    .to_string(),
                base_url: format!("{url}/v1"),
                default_model: creds
                    .get("MASAR_SUBAGENT_MODEL")
                    .unwrap_or("subagent")
                    .to_string(),
            }
        }
        ChatProvider::Lmstudio => Endpoint {
            api_key: "not-needed".into(),
            base_url: trim_slash(
                creds
                    .get("LMSTUDIO_URL")
                    .unwrap_or("http://127.0.0.1:1234/v1"),
            ),
            default_model: creds
                .get("LMSTUDIO_MODEL")
                .unwrap_or("qwen3-30b-a3b")
                .to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn creds(pairs: &[(&str, &str)]) -> Credentials {
        Credentials::from_env(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        )
    }

    #[test]
    fn llm_provider_wins_over_key_order() {
        let c = creds(&[
            ("ANTHROPIC_API_KEY", "a"),
            ("DEEPSEEK_API_KEY", "d"),
            ("LLM_PROVIDER", "deepseek"),
        ]);
        assert_eq!(select_provider(&c), Ok(ChatProvider::Deepseek));
    }

    #[test]
    fn control_without_llm_provider_the_first_key_in_priority_order_wins() {
        let c = creds(&[("OPENAI_API_KEY", "o"), ("DEEPSEEK_API_KEY", "d")]);
        assert_eq!(select_provider(&c), Ok(ChatProvider::Deepseek));
    }

    #[test]
    fn a_bare_provider_variable_does_not_steer() {
        let c = creds(&[("ANTHROPIC_API_KEY", "a"), ("PROVIDER", "deepseek")]);
        assert_eq!(select_provider(&c), Ok(ChatProvider::Anthropic));
    }

    #[test]
    fn no_key_and_no_provider_is_an_error_naming_every_key_var() {
        let err = select_provider(&creds(&[])).unwrap_err();
        assert_eq!(
            err.message,
            "No LLM API key found. Please set ANTHROPIC_API_KEY, DEEPSEEK_API_KEY, KIMI_API_KEY, OPENAI_API_KEY, OPEN_ROUTER_API_KEY."
        );
    }

    #[test]
    fn an_unknown_provider_name_is_refused() {
        assert!(select_provider(&creds(&[("LLM_PROVIDER", "nope")])).is_err());
    }

    #[test]
    fn a_hosted_provider_without_its_key_names_the_variable() {
        let err = endpoint(ChatProvider::Kimi, &creds(&[])).unwrap_err();
        assert!(err
            .message
            .starts_with("KIMI_API_KEY environment variable is not set."));
    }

    #[test]
    fn openai_reads_a_base_url_override_and_control_defaults_to_the_public_host() {
        let with = endpoint(
            ChatProvider::Openai,
            &creds(&[("OPENAI_API_KEY", "k"), ("OPENAI_BASE_URL", "http://x/v1/")]),
        )
        .unwrap();
        assert_eq!(with.base_url, "http://x/v1");
        let without = endpoint(ChatProvider::Openai, &creds(&[("OPENAI_API_KEY", "k")])).unwrap();
        assert_eq!(without.base_url, "https://api.openai.com/v1");
    }
}
