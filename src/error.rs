//! The failure every provider reports: the twin of `IntegrationError` in `@almadar/integrations`.

use std::fmt;

pub const VALIDATION_ERROR: &str = "VALIDATION_ERROR";
pub const SERVICE_ERROR: &str = "SERVICE_ERROR";
pub const NOT_CONFIGURED: &str = "NOT_CONFIGURED";

/// A failed call. `code` is set only where the TS provider throws an `IntegrationError`; an error an SDK
/// raised (HTTP failures, unreadable replies) carries no code, exactly as in TS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationError {
    pub code: Option<String>,
    pub message: String,
}

impl IntegrationError {
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
        }
    }

    pub fn coded(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: Some(code.to_string()),
            message: message.into(),
        }
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::coded(VALIDATION_ERROR, message)
    }
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for IntegrationError {}
