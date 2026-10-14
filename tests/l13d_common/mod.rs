//! The replay harness the L13d provider parity tests share: load `fixtures/<service>/*.json`, run each case
//! through a mock transport that asserts the recorded request and answers with the recorded reply.
#![allow(dead_code)]

use orbital_integrations::CallContext;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine;
use orbital_integrations::wallclock::{parse_iso8601, Clock};
use orbital_integrations::{
    Credentials, HttpRequest, HttpResponse, IntegrationError, Integrations, Provider, Transport,
    TransportError,
};
use serde::Deserialize;
use serde_json::{Map, Value};

pub const MOCK_BASE: &str = "http://mock.test";
pub const REDACTED: &str = "<redacted>";
const DEFAULT_HEADERS: [&str; 7] = [
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
pub struct Page {
    pub hostname: String,
    pub referrer: String,
    #[serde(rename = "screenWidth")]
    pub screen_width: Option<f64>,
}

#[derive(Deserialize, Clone)]
pub struct Fixture {
    pub service: String,
    pub action: String,
    pub case: String,
    pub env: HashMap<String, String>,
    pub params: Map<String, Value>,
    pub exchanges: Vec<Exchange>,
    pub outcome: Outcome,
    #[serde(default)]
    pub now: Option<String>,
    #[serde(default)]
    pub page: Option<Page>,
    #[serde(default)]
    pub extra: Map<String, Value>,
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
            let text = std::fs::read_to_string(path).expect("read fixture");
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        })
        .collect()
}

pub fn fixture(service: &str, case: &str) -> Fixture {
    fixtures(service)
        .into_iter()
        .find(|f| f.case == case)
        .unwrap_or_else(|| panic!("fixture {service}/{case}"))
}

/// The instant a fixture was recorded under, as a fixed clock (the real one when it recorded none).
pub fn clock_of(fixture: &Fixture) -> Clock {
    match fixture.now.as_deref() {
        Some(text) => {
            let ms =
                parse_iso8601(text).unwrap_or_else(|| panic!("{}: bad now {text}", fixture.case));
            Arc::new(move || ms)
        }
        None => orbital_integrations::wallclock::system_clock(),
    }
}

pub struct Replay {
    case: String,
    headers: Vec<String>,
    script: Mutex<VecDeque<Exchange>>,
    body_base64: Option<String>,
    pub sent: Mutex<Vec<HttpRequest>>,
}

impl Replay {
    pub fn new(fixture: &Fixture, extra_headers: &[&str]) -> Arc<Self> {
        let headers = DEFAULT_HEADERS
            .iter()
            .chain(extra_headers.iter())
            .map(|h| h.to_string())
            .collect();
        Arc::new(Self {
            case: fixture.case.clone(),
            headers,
            script: Mutex::new(fixture.exchanges.iter().cloned().collect()),
            body_base64: fixture
                .extra
                .get("bodyBase64")
                .and_then(Value::as_str)
                .map(str::to_string),
            sent: Mutex::new(Vec::new()),
        })
    }

    pub fn unused(&self) -> usize {
        self.script.lock().expect("script").len()
    }

    fn semantic(&self, headers: &[(String, String)]) -> BTreeMap<String, String> {
        headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.clone()))
            .filter(|(k, _)| self.headers.contains(k))
            .map(|(k, v)| {
                if SECRET_HEADERS.contains(&k.as_str()) {
                    (k, REDACTED.to_string())
                } else {
                    (k, v)
                }
            })
            .collect()
    }
}

/// A body compared as JSON when both sides are JSON, else as text.
pub fn same_body(sent: &str, recorded: &str) -> bool {
    match (
        serde_json::from_str::<Value>(sent),
        serde_json::from_str::<Value>(recorded),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => sent == recorded,
    }
}

impl Replay {
    fn expect(&self, request: &HttpRequest) -> Exchange {
        let case = &self.case;
        self.sent.lock().expect("sent").push(request.clone());
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
        assert_eq!(
            self.semantic(&request.headers),
            expected.request.headers,
            "{case}: headers"
        );
        expected
    }
}

fn reply_of(expected: Exchange) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status: expected.response.status,
        headers: expected.response.headers.into_iter().collect(),
        body: expected.response.body.into_bytes(),
    })
}

impl Transport for Replay {
    /// A fixture that records `extra.bodyBase64` holds a binary request body (the text body of an exchange cannot
    /// carry it); every other fixture compares the body as text.
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let expected = self.expect(request);
        match self.body_base64.as_deref() {
            Some(recorded) => assert_eq!(
                base64::engine::general_purpose::STANDARD.encode(&request.body),
                recorded,
                "{}: request body (base64)",
                self.case
            ),
            None => assert!(
                same_body(&request.text(), &expected.request.body),
                "{}: request body\n sent:     {}\n recorded: {}",
                self.case,
                request.text(),
                expected.request.body
            ),
        }
        reply_of(expected)
    }
}

pub fn credentials(fixture: &Fixture) -> Credentials {
    Credentials::from_env(
        fixture
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.replace("{base}", MOCK_BASE)))
            .collect(),
    )
}

/// Run one fixture: `build` makes the provider from the credentials and the replay transport.
pub fn run<F>(
    fixture: &Fixture,
    extra_headers: &[&str],
    build: F,
) -> (Result<Value, IntegrationError>, Arc<Replay>)
where
    F: Fn(
        &Credentials,
        Arc<dyn Transport>,
        &Fixture,
    ) -> Result<Arc<dyn Provider>, IntegrationError>,
{
    let creds = credentials(fixture);
    let replay = Replay::new(fixture, extra_headers);
    let transport: Arc<dyn Transport> = replay.clone();
    let mut integrations = Integrations::new(&creds).expect("mode");
    match build(&creds, transport, fixture) {
        Ok(provider) => integrations.register(&fixture.service, provider),
        Err(e) => integrations.register_failed(&fixture.service, e),
    }
    let params = match serde_json::from_str(
        &Value::Object(fixture.params.clone())
            .to_string()
            .replace("{base}", MOCK_BASE),
    ) {
        Ok(Value::Object(map)) => map,
        other => panic!("{}: params are not an object: {other:?}", fixture.case),
    };
    let result = integrations.call_json(
        &fixture.service,
        &fixture.action,
        &params,
        &CallContext::anonymous(),
    );
    (result, replay)
}

pub fn check(fixture: &Fixture, result: Result<Value, IntegrationError>, replay: &Replay) {
    assert_eq!(
        replay.unused(),
        0,
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

/// Every fixture of `service` replays to the recorded outcome.
pub fn replay_all<F>(service: &str, minimum: usize, extra_headers: &[&str], build: F)
where
    F: Fn(
        &Credentials,
        Arc<dyn Transport>,
        &Fixture,
    ) -> Result<Arc<dyn Provider>, IntegrationError>,
{
    let all = fixtures(service);
    assert!(
        all.len() >= minimum,
        "expected the recorded {service} cases, found {}",
        all.len()
    );
    for fixture in &all {
        let (result, replay) = run(fixture, extra_headers, &build);
        check(fixture, result, &replay);
    }
}

/// No secret reaches a fixture: keys are the literal `test-key`, secret headers are redacted.
pub fn assert_no_secrets(service: &str, secret_env: &[&str]) {
    for fixture in fixtures(service) {
        for (name, value) in &fixture.env {
            if secret_env.contains(&name.as_str()) {
                assert_eq!(value, "test-key", "{}: {name}", fixture.case);
            }
        }
        for exchange in &fixture.exchanges {
            for secret in SECRET_HEADERS {
                if let Some(value) = exchange.request.headers.get(secret) {
                    assert_eq!(value, REDACTED, "{}: {secret}", fixture.case);
                }
            }
        }
    }
}
