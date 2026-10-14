//! The Zod schemas `OAuthIntegration` parses its params with, and Zod's error text
//! (`JSON.stringify(issues, null, 2)`). The service contract has already checked each param's top-level type, so
//! the failures left are an unknown provider and non-string members of an array.

use serde_json::{json, Map, Value};

use super::{AuthorizeInput, OAuthProviderKind, TokenInput};
use crate::error::IntegrationError;

const PROVIDERS: [&str; 3] = ["google", "github", "auth0"];

/// Zod's `getParsedType` name for a JSON value.
fn parsed_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn invalid_type(expected: &str, received: &Value, path: Vec<Value>) -> Value {
    json!({
        "code": "invalid_type",
        "expected": expected,
        "received": parsed_type(received),
        "path": path,
        "message": format!("Expected {expected}, received {}", parsed_type(received)),
    })
}

fn fail(issues: Vec<Value>) -> IntegrationError {
    let text = serde_json::to_string_pretty(&Value::Array(issues)).unwrap_or_default();
    IntegrationError::plain(text)
}

fn provider_issue(
    params: &Map<String, Value>,
    issues: &mut Vec<Value>,
) -> Option<OAuthProviderKind> {
    match params.get("provider") {
        None => {
            issues.push(json!({
                "code": "invalid_type", "expected": "'google' | 'github' | 'auth0'", "received": "undefined",
                "path": ["provider"], "message": "Required",
            }));
            None
        }
        Some(Value::String(name)) => match OAuthProviderKind::parse(name) {
            Some(kind) => Some(kind),
            None => {
                issues.push(json!({
                    "received": name,
                    "code": "invalid_enum_value",
                    "options": PROVIDERS,
                    "path": ["provider"],
                    "message": format!("Invalid enum value. Expected 'google' | 'github' | 'auth0', received '{name}'"),
                }));
                None
            }
        },
        Some(other) => {
            issues.push(json!({
                "code": "invalid_type", "expected": "'google' | 'github' | 'auth0'", "received": parsed_type(other),
                "path": ["provider"], "message": format!("Expected 'google' | 'github' | 'auth0', received {}", parsed_type(other)),
            }));
            None
        }
    }
}

fn string_at(
    params: &Map<String, Value>,
    key: &str,
    required: bool,
    issues: &mut Vec<Value>,
) -> Option<String> {
    match params.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            issues.push(invalid_type("string", other, vec![json!(key)]));
            None
        }
        None if required => {
            issues.push(json!({
                "code": "invalid_type", "expected": "string", "received": "undefined",
                "path": [key], "message": "Required",
            }));
            None
        }
        None => None,
    }
}

fn string_array(params: &Map<String, Value>, key: &str, issues: &mut Vec<Value>) -> Vec<String> {
    match params.get(key) {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for (index, item) in items.iter().enumerate() {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    other => issues.push(invalid_type(
                        "string",
                        other,
                        vec![json!(key), json!(index)],
                    )),
                }
            }
            out
        }
        Some(other) => {
            issues.push(invalid_type("array", other, vec![json!(key)]));
            Vec::new()
        }
        None => {
            issues.push(json!({
                "code": "invalid_type", "expected": "array", "received": "undefined",
                "path": [key], "message": "Required",
            }));
            Vec::new()
        }
    }
}

pub(super) fn authorize_input(
    params: &Map<String, Value>,
) -> Result<AuthorizeInput, IntegrationError> {
    let mut issues = Vec::new();
    let provider = provider_issue(params, &mut issues);
    let scopes = string_array(params, "scopes", &mut issues);
    let redirect_uri = string_at(params, "redirectUri", true, &mut issues);
    let subject = string_at(params, "subject", false, &mut issues);
    match (provider, redirect_uri) {
        (Some(provider), Some(redirect_uri)) if issues.is_empty() => Ok(AuthorizeInput {
            provider,
            scopes,
            redirect_uri,
            subject,
        }),
        _ => Err(fail(issues)),
    }
}

pub(super) fn token_input(params: &Map<String, Value>) -> Result<TokenInput, IntegrationError> {
    let mut issues = Vec::new();
    let code = string_at(params, "code", true, &mut issues);
    let state = string_at(params, "state", true, &mut issues);
    let subject = string_at(params, "subject", false, &mut issues);
    match (code, state) {
        (Some(code), Some(state)) if issues.is_empty() => Ok(TokenInput {
            code,
            state,
            subject,
        }),
        _ => Err(fail(issues)),
    }
}

pub(super) fn string_field(
    params: &Map<String, Value>,
    key: &str,
) -> Result<String, IntegrationError> {
    let mut issues = Vec::new();
    match string_at(params, key, true, &mut issues) {
        Some(value) if issues.is_empty() => Ok(value),
        _ => Err(fail(issues)),
    }
}

pub(super) fn provider_input(
    params: &Map<String, Value>,
) -> Result<OAuthProviderKind, IntegrationError> {
    let mut issues = Vec::new();
    match provider_issue(params, &mut issues) {
        Some(kind) if issues.is_empty() => Ok(kind),
        _ => Err(fail(issues)),
    }
}

pub(super) fn connect_input(
    params: &Map<String, Value>,
) -> Result<(OAuthProviderKind, String, String), IntegrationError> {
    let mut issues = Vec::new();
    let provider = provider_issue(params, &mut issues);
    let code = string_at(params, "code", true, &mut issues);
    let state = string_at(params, "state", true, &mut issues);
    match (provider, code, state) {
        (Some(provider), Some(code), Some(state)) if issues.is_empty() => {
            Ok((provider, code, state))
        }
        _ => Err(fail(issues)),
    }
}
