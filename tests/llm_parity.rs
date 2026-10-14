//! Twin parity: every exchange `@almadar/integrations` recorded for the `llm` provider
//! (`fixtures/llm/*.json`, written by `__tests__/llm-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider through a mock transport. It must send the same requests and produce the same result or error.

use orbital_integrations::CallContext;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orbital_integrations::llm::{self, LlmProvider};
use orbital_integrations::{
    Credentials, HttpRequest, HttpResponse, Integrations, Transport, TransportError,
};
use serde::Deserialize;
use serde_json::{Map, Value};

const MOCK_BASE: &str = "http://mock.test";
const REDACTED: &str = "<redacted>";
const SEMANTIC_HEADERS: [&str; 7] = [
    "content-type",
    "accept",
    "authorization",
    "x-api-key",
    "anthropic-version",
    "anthropic-beta",
    "openai-organization",
];
const SECRET_HEADERS: [&str; 2] = ["authorization", "x-api-key"];

#[derive(Deserialize)]
struct Fixture {
    action: String,
    case: String,
    env: HashMap<String, String>,
    params: Map<String, Value>,
    exchanges: Vec<Exchange>,
    outcome: Outcome,
}

#[derive(Deserialize)]
struct Exchange {
    request: Recorded,
    response: Reply,
}

#[derive(Deserialize)]
struct Recorded {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    body: String,
}

#[derive(Deserialize)]
struct Reply {
    status: u16,
    body: String,
    #[serde(default)]
    headers: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Outcome {
    Ok {
        data: Value,
    },
    Err {
        code: Option<String>,
        message: String,
    },
}

fn fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/llm");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!("fixtures/llm is missing ({e}); record it with ALMADAR_RECORD_FIXTURES")
        })
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path).expect("read fixture");
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        })
        .collect()
}

struct ReplayTransport {
    case: String,
    script: Mutex<std::collections::VecDeque<Exchange>>,
}

fn semantic(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .filter(|(k, _)| SEMANTIC_HEADERS.contains(&k.as_str()))
        .map(|(k, v)| {
            if SECRET_HEADERS.contains(&k.as_str()) {
                (k, REDACTED.to_string())
            } else {
                (k, v)
            }
        })
        .collect()
}

impl Transport for ReplayTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let case = &self.case;
        let expected = self
            .script
            .lock()
            .expect("script lock")
            .pop_front()
            .unwrap_or_else(|| {
                panic!(
                    "{case}: the provider sent an unrecorded request: {} {}",
                    request.method, request.url
                )
            });
        assert_eq!(request.method, expected.request.method, "{case}: method");
        assert_eq!(
            request.url,
            expected.request.url.replace("{base}", MOCK_BASE),
            "{case}: url"
        );
        assert_eq!(
            semantic(&request.headers),
            expected.request.headers,
            "{case}: headers"
        );
        let sent: Value = serde_json::from_str(&request.text())
            .unwrap_or_else(|e| panic!("{case}: sent a non-JSON body ({e}): {}", request.text()));
        let recorded: Value =
            serde_json::from_str(&expected.request.body).expect("recorded body is JSON");
        assert_eq!(sent, recorded, "{case}: request body");
        Ok(HttpResponse {
            status: expected.response.status,
            headers: expected.response.headers.into_iter().collect(),
            body: expected.response.body.into_bytes(),
        })
    }
}

fn run(
    fixture: &Fixture,
) -> (
    Result<Value, orbital_integrations::IntegrationError>,
    Vec<Duration>,
    usize,
) {
    let env: HashMap<String, String> = fixture
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("{base}", MOCK_BASE)))
        .collect();
    let creds = Credentials::from_env(env);
    let script = fixture.exchanges.iter().map(|e| Exchange {
        request: Recorded {
            method: e.request.method.clone(),
            url: e.request.url.clone(),
            headers: e.request.headers.clone(),
            body: e.request.body.clone(),
        },
        response: Reply {
            status: e.response.status,
            body: e.response.body.clone(),
            headers: e.response.headers.clone(),
        },
    });
    let transport = Arc::new(ReplayTransport {
        case: fixture.case.clone(),
        script: Mutex::new(script.collect()),
    });
    let waits = Arc::new(Mutex::new(Vec::new()));
    let recorded_waits = waits.clone();
    let mut integrations = Integrations::new(&creds).expect("mode");
    let dynamic: Arc<dyn Transport> = transport.clone();
    match LlmProvider::new(&creds, dynamic.clone()) {
        Ok(provider) => integrations.register(
            llm::SERVICE,
            Arc::new(provider.with_sleeper(Arc::new(move |d| {
                recorded_waits.lock().expect("waits").push(d)
            }))),
        ),
        Err(e) => integrations.register_failed(llm::SERVICE, e),
    }
    let result = integrations.call_json(
        llm::SERVICE,
        &fixture.action,
        &fixture.params,
        &CallContext::anonymous(),
    );
    let unused = transport.script.lock().expect("script lock").len();
    let waited = waits.lock().expect("waits").clone();
    (result, waited, unused)
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures();
    assert!(
        all.len() >= 30,
        "expected the recorded llm cases, found {}",
        all.len()
    );
    for fixture in &all {
        let (result, _, unused) = run(fixture);
        assert_eq!(
            unused, 0,
            "{}: recorded requests the provider never sent",
            fixture.case
        );
        match (&fixture.outcome, result) {
            (Outcome::Ok { data }, Ok(actual)) => {
                assert_eq!(&actual, data, "{}: result", fixture.case)
            }
            (Outcome::Ok { .. }, Err(e)) => panic!("{}: expected success, got {e:?}", fixture.case),
            (Outcome::Err { .. }, Ok(actual)) => {
                panic!("{}: expected an error, got {actual}", fixture.case)
            }
            (Outcome::Err { code, message }, Err(e)) => {
                assert_eq!(&e.message, message, "{}: error message", fixture.case);
                assert_eq!(&e.code, code, "{}: error code", fixture.case);
            }
        }
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixtures()
        .into_iter()
        .find(|f| f.case == "openai-generate")
        .expect("openai-generate fixture");
    drifted.exchanges[0].request.body = drifted.exchanges[0]
        .request
        .body
        .replace("\"max_tokens\":1024", "\"max_tokens\":512");
    let outcome = std::panic::catch_unwind(|| run(&drifted));
    assert!(
        outcome.is_err(),
        "a different request body must fail the replay"
    );
}

#[test]
fn the_embedding_error_envelope_retries_with_the_recorded_backoff() {
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.case == "lmstudio-embed-provider-error-envelope")
        .expect("fixture");
    let (_, waits, _) = run(&fixture);
    assert_eq!(
        waits,
        vec![Duration::from_millis(500), Duration::from_millis(2000)]
    );
}

#[test]
fn fixtures_hold_no_real_secrets() {
    for fixture in fixtures() {
        for (name, value) in &fixture.env {
            if name.ends_with("_API_KEY") {
                assert_eq!(value, "test-key", "{}: {name}", fixture.case);
            }
        }
        for exchange in &fixture.exchanges {
            for secret in ["authorization", "x-api-key"] {
                if let Some(value) = exchange.request.headers.get(secret) {
                    assert_eq!(value, REDACTED, "{}: {secret}", fixture.case);
                }
            }
        }
    }
}
