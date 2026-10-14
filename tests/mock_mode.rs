//! Mock mode answers every mockable service with `orbital-core`'s registry-shaped value, exactly.

use std::collections::HashMap;

use orbital_core::language::registry::baked_service_registry;
use orbital_core::runtime::mock_answer::mock_answer;
use orbital_integrations::{CallContext, Credentials, Integrations, Mode, UNMOCKED_SERVICES};
use serde_json::{json, Map, Value};

fn mock_integrations() -> Integrations {
    let creds = Credentials::from_env(HashMap::from([(
        "ALMADAR_INTEGRATIONS_MODE".to_string(),
        "mock".to_string(),
    )]));
    let integrations = Integrations::new(&creds).expect("mock mode");
    assert_eq!(integrations.mode(), Mode::Mock);
    integrations
}

#[test]
fn every_mockable_registry_action_returns_cores_mock_value() {
    let integrations = mock_integrations();
    let params = Map::new();
    let mut checked = 0;
    for (service, actions) in &baked_service_registry().actions {
        if UNMOCKED_SERVICES.contains(&service.as_str()) {
            continue;
        }
        for action in actions {
            let got = integrations
                .call_json(service, action, &params, &CallContext::anonymous())
                .unwrap_or_else(|e| panic!("{service}.{action}: {}", e.message));
            let want = mock_answer(service, action, &params).expect("core mock");
            assert_eq!(got, want, "{service}.{action}");
            checked += 1;
        }
    }
    assert!(checked > 200, "only {checked} actions checked");
}

#[test]
fn a_shaped_field_follows_the_request_params() {
    let integrations = mock_integrations();
    let params = json!({"text": "x", "schema": {"type": "object", "properties": {"title": {"type": "string"}}}});
    let Value::Object(params) = params else {
        panic!("object")
    };
    let got = integrations
        .call_json("llm", "extract", &params, &CallContext::anonymous())
        .expect("mock extract");
    assert_eq!(
        got,
        json!({"data": {"title": "mock_title"}, "confidence": 42})
    );
}

#[test]
fn control_an_unknown_action_is_an_error_not_an_invented_answer() {
    let err = mock_integrations()
        .call_json(
            "llm",
            "no-such-action",
            &Map::new(),
            &CallContext::anonymous(),
        )
        .unwrap_err();
    assert!(
        err.message
            .contains("No mock response for action: llm.no-such-action"),
        "{}",
        err.message
    );
}

#[test]
fn control_unmocked_services_never_reach_the_mock() {
    let integrations = mock_integrations();
    let err = integrations
        .call_json(
            "credentials",
            "list",
            &Map::new(),
            &CallContext::anonymous(),
        )
        .unwrap_err();
    assert!(!err.message.contains("No mock response"), "{}", err.message);
}
