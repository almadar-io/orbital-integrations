//! Credential resolution: the tenant credential store first, then the host env, as `configureFromStore`
//! layers `{ ...env, ...store.snapshotEnv() }` in `@almadar/integrations`.

use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Credentials {
    env: HashMap<String, String>,
    store: HashMap<String, String>,
}

impl Credentials {
    pub fn from_env(env: HashMap<String, String>) -> Self {
        Self {
            env,
            store: HashMap::new(),
        }
    }

    pub fn from_process_env() -> Self {
        Self::from_env(std::env::vars().collect())
    }

    pub fn with_store(mut self, store: HashMap<String, String>) -> Self {
        self.store = store;
        self
    }

    /// The value for `name`: a store entry wins over the env; an empty value counts as absent.
    pub fn get(&self, name: &str) -> Option<&str> {
        let value = self.store.get(name).or_else(|| self.env.get(name))?;
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn store_wins_over_env() {
        let creds = Credentials::from_env(map(&[("OPENAI_API_KEY", "env-key")]))
            .with_store(map(&[("OPENAI_API_KEY", "store-key")]));
        assert_eq!(creds.get("OPENAI_API_KEY"), Some("store-key"));
    }

    #[test]
    fn control_env_answers_when_the_store_has_no_entry() {
        let creds = Credentials::from_env(map(&[("OPENAI_API_KEY", "env-key")]))
            .with_store(map(&[("OTHER", "x")]));
        assert_eq!(creds.get("OPENAI_API_KEY"), Some("env-key"));
    }

    #[test]
    fn an_empty_store_entry_revokes_the_env_value() {
        let creds = Credentials::from_env(map(&[("OPENAI_API_KEY", "env-key")]))
            .with_store(map(&[("OPENAI_API_KEY", "")]));
        assert_eq!(creds.get("OPENAI_API_KEY"), None);
    }

    #[test]
    fn missing_everywhere_is_none() {
        assert_eq!(Credentials::default().get("OPENAI_API_KEY"), None);
    }
}
