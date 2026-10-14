//! Shared by the infrastructure parity tests (`redis`, `queue`, `docker`, `otel`, `storage`, ...): fixture
//! loading, a scripted clock and random source, and a transport that replays the recorded HTTP exchanges.
#![allow(dead_code)]

use orbital_integrations::CallContext;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use orbital_integrations::infra::Rand;
use orbital_integrations::wallclock::Clock;
use orbital_integrations::{
    HttpRequest, HttpResponse, IntegrationError, Transport, TransportError,
};
use serde::Deserialize;
use serde_json::{Map, Value};

pub const MOCK_BASE: &str = "http://mock.test";
pub const REDACTED: &str = "<redacted>";
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

#[derive(Deserialize, Clone)]
pub struct Fixture {
    pub case: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub setup: Value,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Deserialize, Clone)]
pub struct Step {
    pub action: String,
    pub params: Map<String, Value>,
    #[serde(rename = "clockMs")]
    pub clock_ms: i64,
    #[serde(default)]
    pub randoms: Vec<f64>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub exchanges: Vec<Exchange>,
    pub outcome: Outcome,
}

#[derive(Deserialize, Clone)]
pub struct Exchange {
    pub request: Recorded,
    pub response: Reply,
}

#[derive(Deserialize, Clone)]
pub struct Recorded {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

#[derive(Deserialize, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Outcome {
    Ok {
        data: Value,
    },
    Err {
        code: Option<String>,
        message: String,
    },
}

pub fn fixtures(service: &str) -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(service);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!("fixtures/{service} is missing ({e}); record it with ALMADAR_RECORD_FIXTURES")
        })
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| {
            let text = expand_placeholders(std::fs::read_to_string(path).expect("read fixture"));
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        })
        .collect()
}

/// Recorded fixtures keep the API key prefix as a placeholder so a key-shaped literal never reaches git.
pub fn expand_placeholders(text: String) -> String {
    text.replace("{{API_KEY_PREFIX}}", orbital_integrations::apikeys::API_KEY_PREFIX)
}

pub fn fixture(service: &str, case: &str) -> Fixture {
    fixtures(service)
        .into_iter()
        .find(|f| f.case == case)
        .unwrap_or_else(|| panic!("fixtures/{service}/{case}.json is missing"))
}

/// Fails the test unless `result` is the recorded outcome (same data, or same message and code).
pub fn check_outcome(label: &str, expected: &Outcome, result: Result<Value, IntegrationError>) {
    match (expected, result) {
        (Outcome::Ok { data }, Ok(actual)) => assert_eq!(&actual, data, "{label}: result"),
        (Outcome::Ok { .. }, Err(e)) => panic!("{label}: expected success, got {e:?}"),
        (Outcome::Err { .. }, Ok(actual)) => panic!("{label}: expected an error, got {actual}"),
        (Outcome::Err { code, message }, Err(e)) => {
            assert_eq!(&e.message, message, "{label}: error message");
            assert_eq!(&e.code, code, "{label}: error code");
        }
    }
}

/// A clock a test moves by hand: the handle sets the instant, `clock()` is what the provider reads.
pub struct ScriptedClock(Arc<AtomicI64>);

impl ScriptedClock {
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(0)))
    }
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn clock(&self) -> Clock {
        let cell = self.0.clone();
        Arc::new(move || cell.load(Ordering::SeqCst))
    }
}

pub struct ScriptedRand(Mutex<VecDeque<f64>>);

impl ScriptedRand {
    pub fn new() -> Self {
        Self(Mutex::new(VecDeque::new()))
    }
    pub fn load(&self, values: &[f64]) {
        *self.0.lock().expect("rand") = values.iter().copied().collect();
    }
    pub fn unused(&self) -> usize {
        self.0.lock().expect("rand").len()
    }
}

impl Rand for ScriptedRand {
    fn unit(&self) -> f64 {
        self.0
            .lock()
            .expect("rand")
            .pop_front()
            .expect("the provider drew more random values than the recording")
    }
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

/// Replays one case's recorded exchanges, asserting the provider sends the same requests.
pub struct ReplayTransport {
    case: Mutex<String>,
    script: Mutex<VecDeque<Exchange>>,
}

impl ReplayTransport {
    pub fn new() -> Self {
        Self {
            case: Mutex::new(String::new()),
            script: Mutex::new(VecDeque::new()),
        }
    }

    pub fn load(&self, case: &str, exchanges: &[Exchange]) {
        *self.case.lock().expect("case") = case.to_string();
        *self.script.lock().expect("script") = exchanges.iter().cloned().collect();
    }

    pub fn unused(&self) -> usize {
        self.script.lock().expect("script").len()
    }
}

impl Transport for ReplayTransport {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let case = self.case.lock().expect("case").clone();
        let expected = self
            .script
            .lock()
            .expect("script")
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
        let mut want = expected.request.headers.clone();
        if let Some(auth) = want.get("authorization") {
            assert_eq!(auth, REDACTED, "{case}: recorded authorization is redacted");
        }
        let got = semantic(&request.headers);
        for (name, value) in &want {
            if SECRET_HEADERS.contains(&name.as_str()) {
                assert!(got.contains_key(name), "{case}: header {name} missing");
            } else {
                assert_eq!(got.get(name), Some(value), "{case}: header {name}");
            }
        }
        want.retain(|name, _| !SECRET_HEADERS.contains(&name.as_str()));
        for name in got.keys() {
            assert!(
                SECRET_HEADERS.contains(&name.as_str()) || want.contains_key(name),
                "{case}: unexpected header {name}"
            );
        }
        match (
            serde_json::from_str::<Value>(&request.text()),
            serde_json::from_str::<Value>(&expected.request.body),
        ) {
            (Ok(sent), Ok(recorded)) => assert_eq!(sent, recorded, "{case}: request body"),
            _ => assert_eq!(
                request.text(),
                expected.request.body,
                "{case}: request body"
            ),
        }
        Ok(HttpResponse {
            status: expected.response.status,
            headers: expected.response.headers.into_iter().collect(),
            body: expected.response.body.into_bytes(),
        })
    }
}

/// Plays every step of `fixture` through `integrations`: the clock and random draws the step was recorded under,
/// the recorded HTTP exchanges (when the provider talks HTTP), and the same result or error.
pub fn replay(
    fixture: &Fixture,
    service: &str,
    integrations: &orbital_integrations::Integrations,
    clock: &ScriptedClock,
    rand: &ScriptedRand,
    transport: Option<&ReplayTransport>,
) {
    for (index, step) in fixture.steps.iter().enumerate() {
        let label = format!("{} step {index} ({})", fixture.case, step.action);
        clock.set(step.clock_ms);
        rand.load(&step.randoms);
        if let Some(transport) = transport {
            transport.load(&label, &step.exchanges);
        }
        let result = integrations.call_json(
            service,
            &step.action,
            &step.params,
            &CallContext::anonymous(),
        );
        check_outcome(&label, &step.outcome, result);
        assert_eq!(
            rand.unused(),
            0,
            "{label}: recorded random draws never made"
        );
        if let Some(transport) = transport {
            assert_eq!(
                transport.unused(),
                0,
                "{label}: recorded requests the provider never sent"
            );
        }
    }
}

/// Fails if a fixture carries anything but a test credential.
pub fn assert_no_secrets(fixtures: &[Fixture]) {
    for fixture in fixtures {
        for (name, value) in &fixture.env {
            if name.ends_with("_API_KEY") || name.ends_with("_SECRET_ACCESS_KEY") {
                assert_eq!(value, "test-key", "{}: {name}", fixture.case);
            }
        }
        for step in &fixture.steps {
            for exchange in &step.exchanges {
                for secret in SECRET_HEADERS {
                    if let Some(value) = exchange.request.headers.get(secret) {
                        assert_eq!(value, REDACTED, "{}: {secret}", fixture.case);
                    }
                }
            }
        }
    }
}
