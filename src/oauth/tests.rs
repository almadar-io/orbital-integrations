use serde_json::json;
use url::Url;

use super::oidc::{discovery_url, has_challenge, js_parse_float};
use super::zod;
use crate::transport::HttpResponse;

fn challenge(value: &str) -> HttpResponse {
    HttpResponse {
        status: 401,
        headers: vec![("WWW-Authenticate".into(), value.into())],
        body: Vec::new(),
    }
}

#[test]
fn the_discovery_document_sits_under_the_issuer_path() {
    let url = |issuer: &str| discovery_url(&Url::parse(issuer).expect("issuer")).to_string();
    assert_eq!(
        url("https://accounts.google.com"),
        "https://accounts.google.com/.well-known/openid-configuration"
    );
    assert_eq!(
        url("https://idp.example.com/tenant"),
        "https://idp.example.com/tenant/.well-known/openid-configuration"
    );
    assert_eq!(
        url("https://idp.example.com/tenant/"),
        "https://idp.example.com/tenant/.well-known/openid-configuration"
    );
}

#[test]
fn parse_float_reads_a_numeric_prefix_like_js() {
    assert_eq!(js_parse_float("120"), 120.0);
    assert_eq!(js_parse_float(" 3.5e2xyz"), 350.0);
    assert_eq!(js_parse_float(".5"), 0.5);
    assert_eq!(js_parse_float("-7"), -7.0);
    assert!(js_parse_float("abc").is_nan());
    assert!(js_parse_float("").is_nan());
}

#[test]
fn a_challenge_needs_a_scheme_token_first() {
    assert!(has_challenge(&challenge("Bearer error=\"invalid_token\"")));
    assert!(has_challenge(&challenge("Bearer")));
    assert!(!has_challenge(&challenge("")));
    assert!(!has_challenge(&challenge("=oops")));
    assert!(!has_challenge(&HttpResponse {
        status: 401,
        headers: vec![],
        body: Vec::new()
    }));
}

fn params(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

#[test]
fn zod_reports_every_issue_in_schema_order() {
    let error = zod::authorize_input(&params(
        json!({ "provider": "x", "scopes": ["a", 1], "redirectUri": "u" }),
    ))
    .err()
    .expect("invalid");
    let issues: serde_json::Value = serde_json::from_str(&error.message).expect("json issues");
    assert_eq!(issues[0]["code"], "invalid_enum_value");
    assert_eq!(issues[1]["path"], json!(["scopes", 1]));
    assert_eq!(issues[1]["message"], "Expected string, received number");
    assert_eq!(issues.as_array().map(Vec::len), Some(2));
}

#[test]
fn control_valid_params_parse_without_issues() {
    let input = zod::authorize_input(&params(json!({ "provider": "github", "scopes": ["repo"], "redirectUri": "https://x/cb", "subject": "u" })))
        .expect("valid");
    assert_eq!(input.scopes, vec!["repo".to_string()]);
    assert_eq!(input.subject.as_deref(), Some("u"));
}
