//! Twin parity for the node leg of `knowledge`: every case `__tests__/knowledge-recorded-exchanges.test.ts`
//! recorded (`fixtures/knowledge/*.json`) is replayed against the Rust actions over the Firestore REST store.
//! The Firestore requests, the call's result or error, and the entropy the call drew all have to match.

use orbital_core::effects::ServiceCaller;
use orbital_integrations::CallContext;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use orbital_integrations::iconify::IconifyProvider;
use orbital_integrations::knowledge::actions::explain_prompt::VisualizationDomain;
use orbital_integrations::knowledge::actions::feed::ModelFeedSources;
use orbital_integrations::knowledge::actions::icon_sources::IconifyIconSources;
use orbital_integrations::knowledge::actions::model::OpenAiCompatModels;
use orbital_integrations::knowledge::actions::social_sources::ModelSocialSources;
use orbital_integrations::knowledge::actions::{action_table, Env, NoEmbedder};
use orbital_integrations::knowledge::firestore::{FirestoreConfig, FirestoreStore};
use orbital_integrations::knowledge::memory::MemoryStore;
use orbital_integrations::knowledge::store::ModelPort;
use orbital_integrations::knowledge::KnowledgeProvider;
use orbital_integrations::wikimedia::WikimediaProvider;
use orbital_integrations::Credentials;
use orbital_integrations::{
    HttpRequest, HttpResponse, IntegrationError, Provider, Transport, TransportError,
};
use serde::Deserialize;
use serde_json::{Map, Value};

const MOCK_HOST: &str = "mock.test";
const MOCK_BASE: &str = "http://mock.test";
const REDACTED: &str = "<redacted>";
const HEADERS: [&str; 3] = ["content-type", "accept", "authorization"];

#[derive(Deserialize, Clone)]
struct Fixture {
    action: String,
    case: String,
    #[serde(default)]
    env: std::collections::HashMap<String, String>,
    params: Map<String, Value>,
    exchanges: Vec<Exchange>,
    outcome: Outcome,
    principal: Option<String>,
    now: f64,
    uuids: Vec<String>,
    #[serde(rename = "randomBytes")]
    random_bytes: Vec<Vec<u64>>,
    #[serde(default)]
    progress: Vec<Value>,
    #[serde(default)]
    visualizations: Vec<Visualization>,
}

#[derive(Deserialize, Clone)]
struct Visualization {
    #[serde(rename = "type")]
    domain_type: String,
    description: String,
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
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Outcome {
    Ok {
        data: Value,
    },
    Err {
        code: Option<Value>,
        message: String,
    },
}

fn fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/knowledge");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!("fixtures/knowledge is missing ({e}); record it with ALMADAR_RECORD_FIXTURES")
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

/// Answers each request with the recorded reply of the first unconsumed exchange that has the same method, url and body.
struct Replay {
    case: String,
    script: Mutex<Vec<Exchange>>,
}

fn semantic(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .filter(|(k, _)| HEADERS.contains(&k.as_str()))
        .map(|(k, v)| {
            if k == "authorization" {
                (k, REDACTED.to_string())
            } else {
                (k, v)
            }
        })
        .collect()
}

fn same_body(sent: &str, recorded: &str) -> bool {
    match (
        serde_json::from_str::<Value>(sent),
        serde_json::from_str::<Value>(recorded),
    ) {
        (Ok(a), Ok(b)) => normalize(a) == normalize(b),
        _ => sent == recorded,
    }
}

/// Where two JSON values first differ, as `path: rust != recorded` lines.
fn json_diff(sent: &Value, recorded: &Value, path: &str, out: &mut Vec<String>) {
    if out.len() >= 8 {
        return;
    }
    match (sent, recorded) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys().filter(|k| !a.contains_key(*k))) {
                let at = format!("{path}/{key}");
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => json_diff(x, y, &at, out),
                    (Some(x), None) => out.push(format!("{at}: only in rust: {x}")),
                    (None, Some(y)) => out.push(format!("{at}: only in the recording: {y}")),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(format!(
                    "{path}: {} items in rust, {} recorded",
                    a.len(),
                    b.len()
                ));
            }
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                json_diff(x, y, &format!("{path}/{i}"), out);
            }
        }
        _ if sent != recorded => out.push(format!("{path}: {sent} != {recorded}")),
        _ => {}
    }
}

impl Transport for Replay {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let case = &self.case;
        let mut script = self.script.lock().expect("script");
        let at = script.iter().position(|e| {
            e.request.method == request.method
                && e.request.url.replace("{base}", MOCK_BASE) == request.url
                && same_body(&request.text(), &e.request.body)
        });
        let Some(at) = at else {
            let mut diffs = Vec::new();
            if let Some(near) = script.iter().find(|e| {
                e.request.method == request.method
                    && e.request.url.replace("{base}", MOCK_BASE) == request.url
            }) {
                if let (Ok(sent), Ok(recorded)) = (
                    serde_json::from_str::<Value>(&request.text()),
                    serde_json::from_str::<Value>(&near.request.body),
                ) {
                    json_diff(&normalize(sent), &normalize(recorded), "", &mut diffs);
                }
            }
            panic!(
                "{case}: the provider sent an unrecorded request: {} {}\ndiffers from the recorded one at:\n{}\n({} recorded request(s) left)",
                request.method,
                request.url,
                diffs.join("\n"),
                script.len()
            );
        };
        let expected = script.remove(at);
        assert_eq!(
            semantic(&request.headers),
            expected.request.headers,
            "{case}: headers"
        );
        Ok(HttpResponse {
            status: expected.response.status,
            headers: Vec::new(),
            body: expected.response.body.into_bytes(),
        })
    }
}

/// JS-number-shaped JSON: whole floats become integers so `3.0` and `3` compare equal.
fn normalize(value: Value) -> Value {
    match value {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 9e15 => Value::from(f as i64),
            _ => Value::Number(n),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(normalize).collect()),
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, normalize(v))).collect())
        }
        other => other,
    }
}

fn queue(items: Vec<String>, what: &'static str) -> Arc<dyn Fn() -> String + Send + Sync> {
    let items = Mutex::new(VecDeque::from(items));
    Arc::new(move || {
        items
            .lock()
            .expect("queue")
            .pop_front()
            .unwrap_or_else(|| panic!("the call drew more {what} than the recording did"))
    })
}

struct Run {
    result: Result<Value, IntegrationError>,
    unused: usize,
    progress: Vec<Value>,
}

fn run(fx: &Fixture) -> Run {
    let transport = Arc::new(Replay {
        case: fx.case.clone(),
        script: Mutex::new(fx.exchanges.clone()),
    });
    let now = fx.now as i64;
    let tokens: Vec<String> = fx
        .random_bytes
        .iter()
        .filter(|d| d[0] == 18)
        .map(|d| URL_SAFE_NO_PAD.encode(vec![d[1] as u8; 18]))
        .collect();
    // the SDK draws its own ids (and a tag per request) from the same entropy, so the ids a call created are read off its writes
    let auto_ids: Vec<String> = fx
        .exchanges
        .iter()
        .filter_map(|e| serde_json::from_str::<Value>(&e.request.body).ok())
        .flat_map(|body| {
            body.get("writes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|write| write.pointer("/currentDocument/exists") == Some(&Value::Bool(false)))
        .filter_map(|write| {
            write
                .pointer("/update/name")
                .and_then(Value::as_str)
                .and_then(|n| n.rsplit('/').next())
                .map(str::to_string)
        })
        .collect();
    let store = Arc::new(
        FirestoreStore::new(
            FirestoreConfig::emulator(MOCK_HOST, "demo-kflow"),
            transport.clone(),
        )
        .with_clock(Arc::new(move || now))
        .with_id_generator(queue(auto_ids, "Firestore ids")),
    );
    let creds = Credentials::from_env(fx.env.clone());
    let models: Arc<dyn ModelPort> = Arc::new(OpenAiCompatModels::new(&creds, transport.clone()));
    let deps = store.deps(Arc::new(MemoryStore::new()), models.clone());
    let env = Env {
        uuid: queue(fx.uuids.clone(), "uuids"),
        token: queue(tokens, "tokens"),
        icons: Arc::new(IconifyIconSources::new(Arc::new(IconifyProvider::new(
            &creds,
            transport.clone(),
        )))),
        embedder: Arc::new(NoEmbedder),
        feed: Arc::new(ModelFeedSources {
            models: models.clone(),
        }),
        social: Arc::new(ModelSocialSources {
            models,
            wikimedia: Arc::new(WikimediaProvider::new(&creds, transport.clone())),
        }),
        visualizations: fx
            .visualizations
            .iter()
            .map(|v| VisualizationDomain {
                domain_type: v.domain_type.clone(),
                description: v.description.clone(),
            })
            .collect(),
    };
    let progress = Arc::new(Mutex::new(Vec::new()));
    let sink = progress.clone();
    let provider =
        KnowledgeProvider::new(deps, action_table(&env)).with_progress(Arc::new(move |m| {
            sink.lock()
                .expect("progress")
                .push(serde_json::to_value(m).expect("progress is JSON"))
        }));
    let ctx = CallContext {
        caller: fx.principal.clone().map(|principal| ServiceCaller {
            principal,
            role: None,
        }),
    };
    let result = provider.call(&fx.action, &fx.params, &ctx);
    let unused = transport.script.lock().expect("script").len();
    let progress = progress.lock().expect("progress").clone();
    Run {
        result,
        unused,
        progress,
    }
}

fn check(fx: &Fixture) -> Result<(), String> {
    let Run {
        result,
        unused,
        progress,
    } = run(fx);
    match (&fx.outcome, result) {
        (Outcome::Ok { data }, Ok(actual)) => {
            if normalize(actual.clone()) != normalize(data.clone()) {
                return Err(format!("result differs\n  rust: {actual}\n  ts:   {data}"));
            }
        }
        (Outcome::Ok { .. }, Err(e)) => return Err(format!("expected success, got {e:?}")),
        (Outcome::Err { .. }, Ok(actual)) => {
            return Err(format!("expected an error, got {actual}"))
        }
        (Outcome::Err { code, message }, Err(e)) => {
            if &e.message != message {
                return Err(format!(
                    "error message differs\n  rust: {}\n  ts:   {message}",
                    e.message
                ));
            }
            // a raw SDK error carries its numeric HTTP status as `code`; the Rust error holds string codes only
            let comparable = !matches!(code, Some(Value::Number(_)));
            let expected = code.as_ref().and_then(Value::as_str).map(str::to_string);
            if comparable && e.code != expected {
                return Err(format!(
                    "error code differs\n  rust: {:?}\n  ts:   {expected:?}",
                    e.code
                ));
            }
        }
    }
    if unused != 0 {
        return Err(format!(
            "{unused} recorded request(s) the provider never sent"
        ));
    }
    if progress != fx.progress {
        return Err(format!(
            "progress differs\n  rust: {}\n  ts:   {}",
            Value::Array(progress),
            Value::Array(fx.progress.clone())
        ));
    }
    Ok(())
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures();
    assert!(
        all.len() >= 120,
        "expected the recorded knowledge cases, found {}",
        all.len()
    );
    let mut failures = Vec::new();
    for fx in &all {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(fx)));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(message)) => failures.push(format!("{}: {message}", fx.case)),
            Err(panic) => {
                let text = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_string()))
                    .unwrap_or_else(|| "panicked".to_string());
                failures.push(format!("{}: {text}", fx.case));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        all.len(),
        failures.join("\n---\n")
    );
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixtures()
        .into_iter()
        .find(|f| f.case == "preferences-set")
        .expect("preferences-set fixture");
    drifted.exchanges[1].request.body = drifted.exchanges[1]
        .request
        .body
        .replace("deepseek-v4-flash", "drifted-model");
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&drifted))).is_err()
            || check(&drifted).is_err(),
        "a different request body must fail the replay"
    );
}

#[test]
fn control_a_result_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixtures()
        .into_iter()
        .find(|f| f.case == "concept-detail")
        .expect("concept-detail fixture");
    if let Outcome::Ok { data } = &mut drifted.outcome {
        data["pathTitle"] = Value::String("another title".to_string());
    }
    assert!(
        check(&drifted).is_err(),
        "a different result must fail the replay"
    );
    let mut unsent = fixtures()
        .into_iter()
        .find(|f| f.case == "preferences-set")
        .expect("preferences-set fixture");
    unsent.exchanges.push(unsent.exchanges[0].clone());
    assert!(
        check(&unsent).is_err(),
        "a recorded request the provider never sends must fail the replay"
    );
}

#[test]
fn fixtures_hold_no_real_secrets() {
    for fx in fixtures() {
        for exchange in &fx.exchanges {
            if let Some(value) = exchange.request.headers.get("authorization") {
                assert_eq!(value, REDACTED, "{}: authorization", fx.case);
            }
        }
    }
}
