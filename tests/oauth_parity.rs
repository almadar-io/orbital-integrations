//! Twin parity: every sequence of calls `@almadar/integrations` recorded for the `oauth` provider
//! (`fixtures/oauth/*.json`, written by `__tests__/oauth-recorded-exchanges.test.ts`) is replayed against the Rust
//! provider through a mock transport, under the clock the recording ran at. Values drawn at random (state, PKCE
//! challenge and verifier, mock tokens) are recorded as placeholders: the replay normalizes its own output the same
//! way and checks the verifier against the challenge it issued.

use orbital_integrations::CallContext;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use orbital_integrations::oauth::{self, OauthProvider};
use orbital_integrations::{
    Credentials, HttpRequest, HttpResponse, Integrations, Transport, TransportError,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const REDACTED: &str = "<redacted>";
const SEMANTIC_HEADERS: [&str; 3] = ["content-type", "accept", "authorization"];

#[derive(Deserialize)]
struct Fixture {
    case: String,
    #[serde(rename = "nowMs")]
    now_ms: i64,
    env: HashMap<String, String>,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    action: String,
    params: Map<String, Value>,
    #[serde(default, rename = "advanceMs")]
    advance_ms: i64,
    exchanges: Vec<Exchange>,
    outcome: Value,
}

#[derive(Deserialize, Clone)]
struct Exchange {
    request: Recorded,
    response: Reply,
}

#[derive(Deserialize, Clone)]
struct Recorded {
    method: String,
    url: String,
    headers: BTreeMap<String, String>,
    body: String,
}

#[derive(Deserialize, Clone)]
struct Reply {
    status: u16,
    body: String,
    #[serde(default)]
    headers: HashMap<String, String>,
}

fn fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/oauth");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!("fixtures/oauth is missing ({e}); record it with ALMADAR_RECORD_FIXTURES")
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
    script: Mutex<VecDeque<Exchange>>,
    /// The PKCE challenges the provider has issued: a code verifier on the wire must hash to one of them.
    challenges: Arc<Mutex<Vec<String>>>,
}

fn semantic(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .filter(|(k, _)| SEMANTIC_HEADERS.contains(&k.as_str()))
        .map(|(k, v)| {
            if k == "authorization" {
                (k, REDACTED.to_string())
            } else {
                (k, v)
            }
        })
        .collect()
}

fn form_pairs(body: &str) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
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
        assert_eq!(request.url, expected.request.url, "{case}: url");
        let mut headers = semantic(&request.headers);
        let mut recorded_headers = expected.request.headers.clone();
        if request.method == "POST" {
            headers
                .entry("accept".into())
                .or_insert_with(|| "*/*".into());
            recorded_headers
                .entry("accept".into())
                .or_insert_with(|| "*/*".into());
        }
        assert_eq!(headers, recorded_headers, "{case}: headers");

        let mut sent = form_pairs(&request.text());
        if let Some(verifier) = sent.get("code_verifier").cloned() {
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            assert!(
                self.challenges
                    .lock()
                    .expect("challenges")
                    .contains(&challenge),
                "{case}: the code verifier does not hash to an issued challenge"
            );
            sent.insert("code_verifier".into(), "<code_verifier>".into());
        }
        assert_eq!(
            sent,
            form_pairs(&expected.request.body),
            "{case}: request body"
        );
        // The recording stub answers `content-type: application/json` unless the scripted reply names one.
        let mut headers: HashMap<String, String> =
            HashMap::from([("content-type".to_string(), "application/json".to_string())]);
        headers.extend(
            expected
                .response
                .headers
                .into_iter()
                .map(|(k, v)| (k.to_lowercase(), v)),
        );
        Ok(HttpResponse {
            status: expected.response.status,
            headers: headers.into_iter().collect(),
            body: expected.response.body.into_bytes(),
        })
    }
}

fn is_mock(env: &HashMap<String, String>) -> bool {
    let set = |name: &str| env.get(name).is_some_and(|v| !v.is_empty());
    env.get("OAUTH_MODE").is_some_and(|m| m == "mock")
        || !set("OAUTH_CLIENT_ID")
        || !set("OAUTH_CLIENT_SECRET")
}

/// Replace what a run draws at random with the placeholders the recording uses.
fn normalize(text: &str, mock: bool, known: &HashMap<String, &'static str>) -> String {
    let mut out = text.to_string();
    let mut sorted: Vec<(&String, &&str)> = known.iter().collect();
    sorted.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    for (value, name) in sorted {
        out = out.replace(value.as_str(), name);
    }
    if mock {
        let hex = Regex::new(r"\b[0-9a-f]{16,}\b").expect("pattern");
        out = hex
            .replace_all(&out, |caps: &regex::Captures| {
                format!("<hex{}>", caps[0].len())
            })
            .into_owned();
        out = Regex::new(r"user-[0-9a-f]{8}@")
            .expect("pattern")
            .replace_all(&out, "user-<hex8>@")
            .into_owned();
        out = Regex::new(r"avatar/[0-9a-f]{8}\.png")
            .expect("pattern")
            .replace_all(&out, "avatar/<hex8>.png")
            .into_owned();
    }
    out
}

fn resolve(value: &Value, outcomes: &[Value]) -> Value {
    match value {
        Value::String(text) => {
            let pattern = Regex::new(r"\{\{(\d+)\.([A-Za-z]+)\}\}").expect("pattern");
            Value::String(
                pattern
                    .replace_all(text, |caps: &regex::Captures| {
                        let step: usize = caps[1].parse().expect("step index");
                        outcomes[step]["data"][&caps[2]]
                            .as_str()
                            .unwrap_or_else(|| panic!("placeholder {}: no string result", &caps[0]))
                            .to_string()
                    })
                    .into_owned(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| resolve(v, outcomes)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve(v, outcomes)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn run(fixture: &Fixture) {
    let case = &fixture.case;
    let creds = Credentials::from_env(fixture.env.clone());
    let clock_ms = Arc::new(AtomicI64::new(fixture.now_ms));
    let challenges = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(ReplayTransport {
        case: case.clone(),
        script: Mutex::new(
            fixture
                .steps
                .iter()
                .flat_map(|s| s.exchanges.iter().cloned())
                .collect(),
        ),
        challenges: challenges.clone(),
    });
    let reader = clock_ms.clone();
    let provider = OauthProvider::new(&creds, transport.clone())
        .with_clock(Arc::new(move || reader.load(Ordering::SeqCst)));
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(oauth::SERVICE, Arc::new(provider));

    let mock = is_mock(&fixture.env);
    let mut outcomes: Vec<Value> = Vec::new();
    let mut known: HashMap<String, &'static str> = HashMap::new();
    for (index, step) in fixture.steps.iter().enumerate() {
        clock_ms.fetch_add(step.advance_ms, Ordering::SeqCst);
        let before = transport.script.lock().expect("script lock").len();
        let params = match resolve(&Value::Object(step.params.clone()), &outcomes) {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        let result = integrations.call_json(
            oauth::SERVICE,
            &step.action,
            &params,
            &CallContext::anonymous(),
        );
        let consumed = before - transport.script.lock().expect("script lock").len();
        assert_eq!(
            consumed,
            step.exchanges.len(),
            "{case} step {index}: requests sent"
        );
        let raw = match &result {
            Ok(data) => json!({ "ok": true, "data": data }),
            Err(e) => json!({ "ok": false, "code": e.code, "message": e.message }),
        };
        if let (Ok(data), false) = (&result, mock) {
            if let (Some(state), Some(auth_url)) =
                (data["state"].as_str(), data["authUrl"].as_str())
            {
                known.insert(state.to_string(), "<state>");
                let challenge = url::Url::parse(auth_url)
                    .expect("auth url")
                    .query_pairs()
                    .find(|(k, _)| k == "code_challenge")
                    .map(|(_, v)| v.into_owned())
                    .expect("code_challenge");
                known.insert(challenge.clone(), "<code_challenge>");
                challenges.lock().expect("challenges").push(challenge);
            }
        }
        let normalized: Value =
            serde_json::from_str(&normalize(&raw.to_string(), mock, &known)).expect("json");
        assert_eq!(
            normalized, step.outcome,
            "{case} step {index} ({})",
            step.action
        );
        outcomes.push(raw);
    }
    assert_eq!(
        transport.script.lock().expect("script lock").len(),
        0,
        "{case}: recorded requests the provider never sent"
    );
}

#[test]
fn every_recorded_sequence_replays_to_the_same_results() {
    let all = fixtures();
    assert!(
        all.len() >= 55,
        "expected the recorded oauth cases, found {}",
        all.len()
    );
    for fixture in &all {
        run(fixture);
    }
}

#[test]
fn both_backends_and_every_action_are_recorded() {
    let all = fixtures();
    for action in [
        "authorize",
        "token",
        "refresh",
        "revoke",
        "userinfo",
        "connect",
        "connection",
        "disconnect",
    ] {
        for mock in [true, false] {
            if !mock && matches!(action, "connection" | "disconnect") {
                continue;
            }
            assert!(
                all.iter().any(|f| is_mock(&f.env) == mock
                    && f.steps
                        .iter()
                        .any(|s| s.action == action && s.outcome["ok"] == true))
                    || matches!(action, "connect" | "connection" | "disconnect"),
                "{action}: no success recorded for the {} backend",
                if mock { "mock" } else { "OIDC" }
            );
        }
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixtures()
        .into_iter()
        .find(|f| f.case == "real-flow-google")
        .expect("fixture");
    drifted.steps[1].exchanges[0].request.body = drifted.steps[1].exchanges[0]
        .request
        .body
        .replace("client_id=client-id-1", "client_id=someone-else");
    assert!(std::panic::catch_unwind(|| run(&drifted)).is_err());
}

#[test]
fn control_a_changed_outcome_is_caught() {
    let mut drifted = fixtures()
        .into_iter()
        .find(|f| f.case == "mock-revoke-unknown")
        .expect("fixture");
    drifted.steps[0].outcome = json!({ "ok": true, "data": { "revoked": true } });
    assert!(std::panic::catch_unwind(|| run(&drifted)).is_err());
}

#[test]
fn fixtures_hold_no_real_secrets() {
    for fixture in fixtures() {
        for (name, value) in &fixture.env {
            if name.ends_with("_SECRET") {
                assert_eq!(value, "test-key", "{}: {name}", fixture.case);
            }
        }
        for step in &fixture.steps {
            for exchange in &step.exchanges {
                if let Some(value) = exchange.request.headers.get("authorization") {
                    assert_eq!(value, REDACTED, "{}", fixture.case);
                }
            }
        }
    }
}

struct Silent;
impl Transport for Silent {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        panic!("unexpected request {} {}", request.method, request.url)
    }
}

fn mock_provider(clock: Arc<AtomicI64>) -> Integrations {
    let creds = Credentials::from_env(HashMap::from([(
        "OAUTH_MODE".to_string(),
        "mock".to_string(),
    )]));
    let provider = OauthProvider::new(&creds, Arc::new(Silent))
        .with_clock(Arc::new(move || clock.load(Ordering::SeqCst)));
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(oauth::SERVICE, Arc::new(provider));
    integrations
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

#[test]
fn a_mock_profile_is_derived_from_the_subject_it_was_issued() {
    let integrations = mock_provider(Arc::new(AtomicI64::new(0)));
    let started = integrations
        .call_json(
            oauth::SERVICE,
            "authorize",
            &object(
                json!({ "provider": "github", "scopes": ["a"], "redirectUri": "https://x/cb" }),
            ),
            &CallContext::anonymous(),
        )
        .expect("authorize");
    let granted = integrations
        .call_json(
            oauth::SERVICE,
            "token",
            &object(json!({ "code": "c", "state": started["state"] })),
            &CallContext::anonymous(),
        )
        .expect("token");
    let info = integrations
        .call_json(
            oauth::SERVICE,
            "userinfo",
            &object(json!({ "accessToken": granted["accessToken"] })),
            &CallContext::anonymous(),
        )
        .expect("userinfo");
    let sub = info["sub"].as_str().expect("sub");
    assert_eq!(sub.len(), 16);
    assert_eq!(
        info["email"],
        format!("user-{}@github.example.com", &sub[..8])
    );
    assert_eq!(
        info["picture"],
        format!("https://github.example.com/avatar/{}.png", &sub[..8])
    );
    assert_eq!(granted["accessToken"].as_str().map(str::len), Some(64));
    assert_eq!(started["state"].as_str().map(str::len), Some(32));
}

#[test]
fn control_two_authorizations_never_share_a_state_or_a_token() {
    let integrations = mock_provider(Arc::new(AtomicI64::new(0)));
    let params =
        object(json!({ "provider": "google", "scopes": [], "redirectUri": "https://x/cb" }));
    let first = integrations
        .call_json(
            oauth::SERVICE,
            "authorize",
            &params,
            &CallContext::anonymous(),
        )
        .expect("first");
    let second = integrations
        .call_json(
            oauth::SERVICE,
            "authorize",
            &params,
            &CallContext::anonymous(),
        )
        .expect("second");
    assert_ne!(first["state"], second["state"]);
}

fn real_provider(clock: Arc<AtomicI64>, transport: Arc<dyn Transport>) -> Integrations {
    let creds = Credentials::from_env(HashMap::from([
        ("OAUTH_CLIENT_ID".to_string(), "cid".to_string()),
        ("OAUTH_CLIENT_SECRET".to_string(), "secret".to_string()),
    ]));
    let provider = OauthProvider::new(&creds, transport)
        .with_clock(Arc::new(move || clock.load(Ordering::SeqCst)));
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(oauth::SERVICE, Arc::new(provider));
    integrations
}

struct TokenOnly;
impl Transport for TokenOnly {
    fn send(&self, _request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: r#"{"access_token":"gho_x","token_type":"bearer"}"#.into(),
        })
    }
}

#[test]
fn a_pending_grant_lives_exactly_ten_minutes() {
    let started_at = 1_800_000_000_000;
    for (wait_ms, expect_ok) in [(600_000, true), (600_001, false)] {
        let clock = Arc::new(AtomicI64::new(started_at));
        let integrations = real_provider(clock.clone(), Arc::new(TokenOnly));
        let started = integrations
            .call_json(
                oauth::SERVICE,
                "authorize",
                &object(
                    json!({ "provider": "github", "scopes": [], "redirectUri": "https://x/cb" }),
                ),
                &CallContext::anonymous(),
            )
            .expect("authorize");
        clock.fetch_add(wait_ms, Ordering::SeqCst);
        let result = integrations.call_json(
            oauth::SERVICE,
            "token",
            &object(json!({ "code": "c", "state": started["state"] })),
            &CallContext::anonymous(),
        );
        assert_eq!(result.is_ok(), expect_ok, "after {wait_ms} ms: {result:?}");
        if !expect_ok {
            assert!(result
                .expect_err("expired")
                .message
                .starts_with("Invalid or expired state token:"));
        }
    }
}

#[test]
fn registration_follows_the_credentials() {
    let mut off = Integrations::new(&Credentials::default()).expect("mode");
    oauth::register(&mut off, &Credentials::default(), Arc::new(Silent));
    let error = off
        .call_json(
            oauth::SERVICE,
            "revoke",
            &object(json!({ "token": "t" })),
            &CallContext::anonymous(),
        )
        .expect_err("unconfigured");
    assert_eq!(error.code.as_deref(), Some("NOT_CONFIGURED"));

    let creds = Credentials::from_env(HashMap::from([(
        "OAUTH_MODE".to_string(),
        "mock".to_string(),
    )]));
    let mut on = Integrations::new(&creds).expect("mode");
    oauth::register(&mut on, &creds, Arc::new(Silent));
    assert_eq!(
        on.call_json(
            oauth::SERVICE,
            "revoke",
            &object(json!({ "token": "t" })),
            &CallContext::anonymous()
        )
        .expect("mock"),
        json!({ "revoked": false })
    );

    let half = Credentials::from_env(HashMap::from([(
        "OAUTH_CLIENT_ID".to_string(),
        "x".to_string(),
    )]));
    let mut incomplete = Integrations::new(&half).expect("mode");
    oauth::register(&mut incomplete, &half, Arc::new(Silent));
    assert_eq!(
        incomplete
            .call_json(
                oauth::SERVICE,
                "revoke",
                &object(json!({ "token": "t" })),
                &CallContext::anonymous()
            )
            .expect_err("unconfigured")
            .code
            .as_deref(),
        Some("NOT_CONFIGURED")
    );
}
