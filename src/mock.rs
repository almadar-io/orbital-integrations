//! Mock mode: the env switch and the seam for the answers.
//!
//! The mock answers come from `orbital_core::runtime::mock_answer`, the one registry-shaped generator every
//! host shares; [`CoreMockResponder`] is the default responder, and a host can still plug its own in.

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use orbital_core::runtime::mock_answer::mock_answer;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Mock,
    Real,
}

pub trait MockResponder: Send + Sync {
    fn respond(
        &self,
        service: &str,
        action: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, IntegrationError>;
}

/// The default responder: the registry-shaped answer from `orbital-core`.
pub struct CoreMockResponder;

impl MockResponder for CoreMockResponder {
    fn respond(
        &self,
        service: &str,
        action: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, IntegrationError> {
        mock_answer(service, action, params).map_err(|e| IntegrationError::plain(e.to_string()))
    }
}

/// `ALMADAR_INTEGRATIONS_MODE=mock` or `USE_MOCK_DATA=true` mean mock; mock is refused under `NODE_ENV=production`.
pub fn mode_from(creds: &Credentials) -> Result<Mode, IntegrationError> {
    let mock = creds.get("ALMADAR_INTEGRATIONS_MODE") == Some("mock")
        || creds.get("USE_MOCK_DATA") == Some("true");
    if !mock {
        return Ok(Mode::Real);
    }
    if creds.get("NODE_ENV") == Some("production") {
        return Err(IntegrationError::plain(
            "Mocked integrations are not permitted when NODE_ENV=production — unset ALMADAR_INTEGRATIONS_MODE/USE_MOCK_DATA.",
        ));
    }
    Ok(Mode::Mock)
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
    fn either_switch_means_mock() {
        assert_eq!(
            mode_from(&creds(&[("ALMADAR_INTEGRATIONS_MODE", "mock")])),
            Ok(Mode::Mock)
        );
        assert_eq!(
            mode_from(&creds(&[("USE_MOCK_DATA", "true")])),
            Ok(Mode::Mock)
        );
    }

    #[test]
    fn control_anything_else_is_real() {
        assert_eq!(mode_from(&creds(&[])), Ok(Mode::Real));
        assert_eq!(
            mode_from(&creds(&[
                ("USE_MOCK_DATA", "false"),
                ("ALMADAR_INTEGRATIONS_MODE", "real")
            ])),
            Ok(Mode::Real)
        );
    }

    #[test]
    fn production_refuses_mock() {
        let err = mode_from(&creds(&[
            ("USE_MOCK_DATA", "true"),
            ("NODE_ENV", "production"),
        ]))
        .unwrap_err();
        assert!(err.message.contains("NODE_ENV=production"));
    }
}
