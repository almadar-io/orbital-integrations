//! Shared replay harness for the lane L13c parity tests (stripe, calendar, drive, email, banking, esign):
//! loads `fixtures/<service>/*.json`, answers the provider's requests from the recording through a mock
//! transport that asserts the same request, and compares the outcome.

#![allow(dead_code)]

use orbital_integrations::CallContext;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use orbital_integrations::{
    Credentials, HttpRequest, HttpResponse, IntegrationError, Integrations, Provider, Transport,
    TransportError,
};
use serde::Deserialize;
use serde_json::{Map, Value};

pub const MOCK_BASE: &str = "http://mock.test";
pub const REDACTED: &str = "<redacted>";
pub const GENERATED: &str = "<generated>";
/// A multipart boundary is random per request; recordings carry this marker and replays normalize to it.
pub const BOUNDARY: &str = "{boundary}";
const SECRET_HEADERS: [&str; 2] = ["authorization", "x-api-key"];
const BASE_HEADERS: [&str; 4] = ["content-type", "accept", "authorization", "x-api-key"];

#[derive(Deserialize, Clone)]
pub struct Fixture {
    pub action: String,
    pub case: String,
    pub env: HashMap<String, String>,
    pub params: Map<String, Value>,
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
            let text = std::fs::read_to_string(path).expect("read fixture");
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        })
        .collect()
}

pub fn fixture_named(service: &str, case: &str) -> Fixture {
    fixtures(service)
        .into_iter()
        .find(|f| f.case == case)
        .unwrap_or_else(|| panic!("fixture {service}/{case}"))
}

/// What the replay asserts besides method and url.
#[derive(Clone)]
pub struct Replay {
    /// Headers compared on top of content-type/accept/authorization/x-api-key.
    pub extra_headers: Vec<&'static str>,
    /// Base used for `{base}` in urls, env values and bodies.
    pub base: String,
}

impl Default for Replay {
    fn default() -> Self {
        Self {
            extra_headers: Vec::new(),
            base: MOCK_BASE.to_string(),
        }
    }
}

pub struct ReplayTransport {
    pub case: String,
    pub replay: Replay,
    pub script: Mutex<VecDeque<Exchange>>,
    pub generated: Mutex<HashMap<String, String>>,
}

fn semantic(headers: &[(String, String)], names: &[&str]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .filter(|(k, _)| names.contains(&k.as_str()))
        .collect()
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    let rest = content_type.strip_prefix("multipart/")?;
    let start = rest.find("boundary=")? + "boundary=".len();
    let boundary: String = rest[start..]
        .chars()
        .take_while(|c| *c != ';' && !c.is_whitespace())
        .collect();
    (!boundary.is_empty()).then_some(boundary)
}

impl ReplayTransport {
    fn names(&self) -> Vec<&str> {
        BASE_HEADERS
            .iter()
            .copied()
            .chain(self.replay.extra_headers.iter().copied())
            .collect()
    }
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
            expected.request.url.replace("{base}", &self.replay.base),
            "{case}: url"
        );
        let mut sent_body = request.text().into_owned();
        let mut sent_headers = semantic(&request.headers, &self.names());
        if let Some(content_type) = sent_headers.get("content-type").cloned() {
            if let Some(boundary) = multipart_boundary(&content_type) {
                sent_headers.insert(
                    "content-type".into(),
                    content_type.replace(&boundary, BOUNDARY),
                );
                sent_body = sent_body.replace(&boundary, BOUNDARY);
            }
        }
        let want = expected.request.headers.clone();
        let mut got = BTreeMap::new();
        for (name, value) in sent_headers {
            if SECRET_HEADERS.contains(&name.as_str()) {
                assert!(!value.is_empty(), "{case}: empty {name}");
                got.insert(name, REDACTED.to_string());
            } else if want.get(&name).is_some_and(|w| w == GENERATED) {
                assert!(!value.is_empty(), "{case}: empty {name}");
                let mut seen = self.generated.lock().expect("generated lock");
                let first = seen.entry(name.clone()).or_insert_with(|| value.clone());
                assert_eq!(
                    *first, value,
                    "{case}: {name} must stay the same across retries"
                );
                got.insert(name, GENERATED.to_string());
            } else {
                got.insert(name, value);
            }
        }
        assert_eq!(got, want, "{case}: headers");
        let recorded_body = expected.request.body.replace("{base}", &self.replay.base);
        match serde_json::from_str::<Value>(&recorded_body) {
            Ok(recorded) if recorded_body.trim_start().starts_with(['{', '[']) => {
                let sent: Value = serde_json::from_str(&sent_body).unwrap_or_else(|e| {
                    panic!("{case}: sent a non-JSON body ({e}): {}", request.text())
                });
                assert_eq!(sent, recorded, "{case}: request body");
            }
            _ => assert_eq!(sent_body, recorded_body, "{case}: request body"),
        }
        Ok(HttpResponse {
            status: expected.response.status,
            headers: expected.response.headers.into_iter().collect(),
            body: expected.response.body.into_bytes(),
        })
    }
}

pub struct Run {
    pub result: Result<Value, IntegrationError>,
    pub unused: usize,
}

pub fn credentials(fixture: &Fixture, replay: &Replay) -> Credentials {
    let env: HashMap<String, String> = fixture
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.replace("{base}", &replay.base)))
        .collect();
    Credentials::from_env(env)
}

pub fn transport_for(fixture: &Fixture, replay: &Replay) -> Arc<ReplayTransport> {
    Arc::new(ReplayTransport {
        case: fixture.case.clone(),
        replay: replay.clone(),
        script: Mutex::new(fixture.exchanges.iter().cloned().collect()),
        generated: Mutex::new(HashMap::new()),
    })
}

/// Register `service` the way a host does: unconfigured when `configured` is false, failed when the
/// constructor refuses, ready otherwise.
pub fn register(
    integrations: &mut Integrations,
    service: &str,
    configured: bool,
    built: impl FnOnce() -> Result<Arc<dyn Provider>, IntegrationError>,
) {
    if !configured {
        integrations.register_unconfigured(service);
        return;
    }
    match built() {
        Ok(provider) => integrations.register(service, provider),
        Err(error) => integrations.register_failed(service, error),
    }
}

pub fn run(
    service: &str,
    fixture: &Fixture,
    replay: &Replay,
    build: impl FnOnce(&mut Integrations, &Credentials, Arc<dyn Transport>),
) -> Run {
    let creds = credentials(fixture, replay);
    let transport = transport_for(fixture, replay);
    let mut integrations = Integrations::new(&creds).expect("mode");
    let dynamic: Arc<dyn Transport> = transport.clone();
    build(&mut integrations, &creds, dynamic);
    let result = integrations.call_json(
        service,
        &fixture.action,
        &fixture.params,
        &CallContext::anonymous(),
    );
    let unused = transport.script.lock().expect("script lock").len();
    Run { result, unused }
}

pub fn assert_outcome(fixture: &Fixture, run: &Run) {
    assert_eq!(
        run.unused, 0,
        "{}: recorded requests the provider never sent",
        fixture.case
    );
    match (&fixture.outcome, &run.result) {
        (Outcome::Ok { data }, Ok(actual)) => assert_eq!(actual, data, "{}: result", fixture.case),
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

pub fn assert_no_secrets(service: &str) {
    for fixture in fixtures(service) {
        for (name, value) in &fixture.env {
            let secretish = ["_KEY", "_TOKEN", "_SECRET", "_SECRET_ID"]
                .iter()
                .any(|s| name.ends_with(s));
            if secretish {
                assert!(
                    value.starts_with("test-") || value.is_empty() || name.ends_with("_SA_KEY"),
                    "{}: {name}",
                    fixture.case
                );
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
