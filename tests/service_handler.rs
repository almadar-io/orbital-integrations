//! `Integrations` answers the kernel's `call-service` through core's own `ServiceHandler` trait.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use orbital_core::effects::ServiceHandler;
use orbital_core::Value;
use orbital_integrations::{
    llm, Credentials, HttpRequest, HttpResponse, IntegrationError, Integrations, MockResponder,
    Mode, Transport, TransportError,
};
use serde_json::{json, Map};

struct Canned {
    seen: Mutex<Vec<HttpRequest>>,
}

impl Transport for Canned {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.seen.lock().expect("seen").push(request.clone());
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: json!({"choices":[{"message":{"content":"hello"}}],"usage":{"prompt_tokens":2,"completion_tokens":3}}).to_string().into_bytes(),
        })
    }
}

fn creds(pairs: &[(&str, &str)]) -> Credentials {
    Credentials::from_env(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>(),
    )
}

fn args(pairs: &[(&str, Value)]) -> Value {
    Value::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn wired(c: &Credentials) -> (Integrations, Arc<Canned>) {
    let transport = Arc::new(Canned {
        seen: Mutex::new(Vec::new()),
    });
    let mut integrations = Integrations::new(c).expect("mode");
    llm::register(&mut integrations, c, transport.clone());
    (integrations, transport)
}

#[test]
fn a_configured_llm_answers_a_kernel_call_service() {
    let c = creds(&[("OPENAI_API_KEY", "k")]);
    let (integrations, transport) = wired(&c);
    let handler: &dyn ServiceHandler = &integrations;
    let out = handler
        .call(
            "llm",
            "generate",
            &args(&[
                ("userPrompt", Value::String("hi".into())),
                ("maxTokens", Value::Number(64.0)),
            ]),
            Some("run-1"),
            None,
        )
        .expect("answer");
    let Value::Object(map) = out else {
        panic!("object result")
    };
    assert_eq!(map.get("content"), Some(&Value::String("hello".into())));
    let sent = transport.seen.lock().expect("seen");
    assert_eq!(sent.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&sent[0].text()).expect("json body");
    assert_eq!(
        body["max_tokens"], 64,
        "kernel numbers reach the wire as integers"
    );
    assert!(handler.cancel("run-1").is_ok());
}

#[test]
fn a_contract_violation_is_refused_before_any_request() {
    let (integrations, transport) = wired(&creds(&[("OPENAI_API_KEY", "k")]));
    let err =
        ServiceHandler::call(&integrations, "llm", "generate", &args(&[]), None, None).unwrap_err();
    assert_eq!(
        err,
        "Validation failed for llm.generate: userPrompt: Missing required parameter: userPrompt"
    );
    assert!(transport.seen.lock().expect("seen").is_empty());
}

#[test]
fn an_unconfigured_llm_names_the_credentials_it_needs() {
    let (integrations, _) = wired(&creds(&[]));
    let err = ServiceHandler::call(
        &integrations,
        "llm",
        "generate",
        &args(&[("userPrompt", Value::String("hi".into()))]),
        None,
        None,
    )
    .unwrap_err();
    assert!(
        err.starts_with("Integration not configured: llm. Set ANTHROPIC_API_KEY"),
        "{err}"
    );
}

#[test]
fn control_an_embedding_key_alone_configures_llm_but_names_the_missing_chat_key() {
    let (integrations, _) = wired(&creds(&[("EMBEDDING_API_KEY", "k")]));
    let err = ServiceHandler::call(
        &integrations,
        "llm",
        "generate",
        &args(&[("userPrompt", Value::String("hi".into()))]),
        None,
        None,
    )
    .unwrap_err();
    assert!(err.starts_with("No LLM API key found."), "{err}");
}

#[test]
fn a_service_without_a_rust_provider_says_so() {
    let (integrations, _) = wired(&creds(&[("OPENAI_API_KEY", "k")]));
    let err = ServiceHandler::call(
        &integrations,
        "stripe",
        "createCustomer",
        &args(&[]),
        None,
        None,
    )
    .unwrap_err();
    assert!(err.contains("no Rust provider"), "{err}");
}

#[test]
fn an_unported_action_is_reported_not_guessed() {
    let (integrations, _) = wired(&creds(&[("OPENAI_API_KEY", "k")]));
    let err = ServiceHandler::call(
        &integrations,
        "llm",
        "call-tools",
        &args(&[
            ("messages", Value::Array(vec![])),
            ("tools", Value::Array(vec![])),
        ]),
        None,
        None,
    )
    .unwrap_err();
    assert!(err.contains("call-tools is not available"), "{err}");
}

#[test]
fn mock_mode_answers_from_core_by_default_and_a_plugged_responder_overrides_it() {
    let c = creds(&[("USE_MOCK_DATA", "true")]);
    let (integrations, transport) = wired(&c);
    assert_eq!(integrations.mode(), Mode::Mock);
    let Value::Object(default_answer) = ServiceHandler::call(
        &integrations,
        "llm",
        "generate",
        &args(&[("userPrompt", Value::String("hi".into()))]),
        None,
        None,
    )
    .expect("core's mock answer") else {
        panic!("object result")
    };
    assert!(!default_answer.contains_key("mocked"));

    struct Fixed;
    impl MockResponder for Fixed {
        fn respond(
            &self,
            service: &str,
            action: &str,
            _params: &Map<String, serde_json::Value>,
        ) -> Result<serde_json::Value, IntegrationError> {
            Ok(json!({ "mocked": format!("{service}.{action}") }))
        }
    }
    let (integrations, _) = wired(&c);
    let integrations = integrations.with_mock_responder(Arc::new(Fixed));
    let Value::Object(map) = ServiceHandler::call(
        &integrations,
        "llm",
        "generate",
        &args(&[("userPrompt", Value::String("hi".into()))]),
        None,
        None,
    )
    .expect("mock answer") else {
        panic!("object result")
    };
    assert_eq!(
        map.get("mocked"),
        Some(&Value::String("llm.generate".into()))
    );
    assert!(
        transport.seen.lock().expect("seen").is_empty(),
        "mock mode never reaches the transport"
    );
}

#[test]
fn the_store_overrides_the_env_key_on_the_wire() {
    let c = creds(&[("OPENAI_API_KEY", "env-key")]).with_store(
        [("OPENAI_API_KEY".to_string(), "store-key".to_string())]
            .into_iter()
            .collect(),
    );
    let (integrations, transport) = wired(&c);
    ServiceHandler::call(
        &integrations,
        "llm",
        "generate",
        &args(&[("userPrompt", Value::String("hi".into()))]),
        None,
        None,
    )
    .expect("answer");
    let sent = transport.seen.lock().expect("seen");
    assert!(sent[0]
        .headers
        .iter()
        .any(|(k, v)| k == "authorization" && v == "Bearer store-key"));
}
