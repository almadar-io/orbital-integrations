//! Where a host keeps each caller's provider grants: the twin of `OAuthConnectionStore`. `connect` hands the
//! store the exchanged tokens; the program only ever sees the connection row.

use serde_json::{json, Map, Value};

use super::OAuthProviderKind;
use crate::error::IntegrationError;

/// A caller's connection to an OAuth provider (`OAuthConnectionRow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRow {
    pub provider: OAuthProviderKind,
    pub connected: bool,
    /// The provider account name; empty when not connected.
    pub account: String,
    /// Provider avatar URL; empty when unknown.
    pub avatar_url: String,
    pub scope: Vec<String>,
    /// ISO timestamp; empty when not connected.
    pub connected_at: String,
}

impl ConnectionRow {
    pub(super) fn to_json(&self) -> Value {
        json!({
            "provider": self.provider.name(),
            "connected": self.connected,
            "account": self.account,
            "avatarUrl": self.avatar_url,
            "scope": self.scope,
            "connectedAt": self.connected_at,
        })
    }
}

/// Keyed by the call's principal (`context.principal`).
pub trait OAuthConnectionStore: Send + Sync {
    /// Keep `grant` (the `token` result: accessToken, refreshToken, expiresIn, tokenType, scope, subject).
    fn save(
        &self,
        principal: &str,
        provider: OAuthProviderKind,
        grant: &Map<String, Value>,
    ) -> Result<ConnectionRow, IntegrationError>;
    fn status(
        &self,
        principal: &str,
        provider: OAuthProviderKind,
    ) -> Result<ConnectionRow, IntegrationError>;
    fn remove(
        &self,
        principal: &str,
        provider: OAuthProviderKind,
    ) -> Result<ConnectionRow, IntegrationError>;
}
