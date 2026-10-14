//! Parameter validation of a call against its service contract: the twin of `validateParams`
//! (`@almadar/integrations` `core/validation.ts`). The contracts are core language data.

use crate::error::IntegrationError;
use orbital_core::language::registry::{baked_service_registry, EmbeddedServiceParam};
use orbital_core::runtime::service_params::{args_of_json, validate_params};
use serde_json::{Map, Value};

/// The installed contract's params for `service.action`, or why there are none.
pub fn declared_params(
    service: &str,
    action: &str,
) -> Result<&'static [EmbeddedServiceParam], String> {
    orbital_core::language_data::language_data().map_err(|e| format!("{}: {e}", e.code()))?;
    let registry = baked_service_registry();
    let actions = registry
        .params
        .get(service)
        .ok_or_else(|| format!("integration: Unknown integration: {service}"))?;
    actions
        .get(action)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("action: Unknown action: {action}"))
}

pub fn validate(
    service: &str,
    action: &str,
    params: &Map<String, Value>,
) -> Result<(), IntegrationError> {
    orbital_core::language_data::language_data().map_err(|e| {
        IntegrationError::validation(format!(
            "Validation failed for {service}.{action}: {}: {e}",
            e.code()
        ))
    })?;
    let issues = validate_params(
        baked_service_registry(),
        service,
        action,
        &args_of_json(params),
    );
    if issues.is_empty() {
        return Ok(());
    }
    let errors: Vec<String> = issues
        .iter()
        .map(|issue| format!("{}: {issue}", issue.param()))
        .collect();
    Err(IntegrationError::validation(format!(
        "Validation failed for {service}.{action}: {}",
        errors.join("; ")
    )))
}
