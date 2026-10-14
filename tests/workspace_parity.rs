//! Twin parity: every action sequence `@almadar/integrations` recorded for the `workspace` service over
//! `storageWorkspaceHost` (`fixtures/workspace/*.json`, written by `__tests__/workspace-recorded-exchanges.test.ts`)
//! is replayed against the Rust provider with an in-memory bucket. Time is pinned, so the Rust twin must produce
//! the same results, the same commit ids, the same bucket object names and contents, and byte-identical git
//! bundles. A second test hands the Rust twin the bucket the TS host left behind and reads its history cold.

use orbital_core::effects::ServiceCaller;
use orbital_integrations::CallContext;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;

use base64::Engine;
use orbital_integrations::workspace::{
    self, Clock, MemoryBucket, StorageWorkspaceHost, WorkspaceBucket, WorkspaceHost,
};
use orbital_integrations::{Credentials, IntegrationError, Integrations};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct Fixture {
    case: String,
    steps: Vec<Step>,
    bucket: BTreeMap<String, Object>,
}

#[derive(Deserialize)]
struct Step {
    action: String,
    params: Map<String, Value>,
    principal: Option<String>,
    cold: bool,
    #[serde(rename = "noHost")]
    no_host: bool,
    #[serde(rename = "atMs")]
    at_ms: i64,
    outcome: Outcome,
    objects: Vec<String>,
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

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Object {
    Text { text: String },
    Bundle { base64: String },
}

impl Object {
    fn bytes(&self) -> Vec<u8> {
        match self {
            Object::Text { text } => text.clone().into_bytes(),
            Object::Bundle { base64 } => base64::engine::general_purpose::STANDARD
                .decode(base64)
                .expect("bundle base64"),
        }
    }
}

fn fixtures() -> Vec<Fixture> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/workspace");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!("fixtures/workspace is missing ({e}); record it with ALMADAR_RECORD_FIXTURES")
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

static DIRS: AtomicUsize = AtomicUsize::new(0);

/// A cache directory removed when dropped.
struct CacheDirs(Vec<PathBuf>);

impl CacheDirs {
    fn fresh(&mut self) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ws-parity-{}-{}",
            std::process::id(),
            DIRS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("cache dir");
        self.0.push(dir.clone());
        dir
    }
}

impl Drop for CacheDirs {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn pinned_clock(now: Arc<AtomicI64>) -> Clock {
    Clock::pinned(Arc::new(move || now.load(Ordering::SeqCst)))
}

fn call(
    host: Option<Arc<dyn WorkspaceHost>>,
    principal: Option<String>,
    action: &str,
    params: &Map<String, Value>,
) -> Result<Value, IntegrationError> {
    let mut integrations = Integrations::new(&Credentials::default()).expect("mode");
    workspace::register(&mut integrations, host);
    let ctx = CallContext {
        caller: principal.map(|principal| ServiceCaller {
            principal,
            role: None,
        }),
    };
    integrations.call_json(workspace::SERVICE, action, params, &ctx)
}

struct Replay {
    results: Vec<Result<Value, IntegrationError>>,
    objects: Vec<Vec<String>>,
    bucket: Arc<MemoryBucket>,
}

fn resolve(value: &Value, results: &[Result<Value, IntegrationError>]) -> Value {
    if let Value::String(s) = value {
        if let Some(n) = s.strip_prefix("$step.") {
            let index: usize = n.parse().expect("step index");
            return match &results[index] {
                Ok(data) => data["oid"].clone(),
                Err(e) => panic!("step {index} failed: {e}"),
            };
        }
    }
    value.clone()
}

fn replay(fixture: &Fixture, shift_ms: i64) -> Replay {
    let mut dirs = CacheDirs(Vec::new());
    let bucket = Arc::new(MemoryBucket::new());
    let now = Arc::new(AtomicI64::new(0));
    let new_host = |dirs: &mut CacheDirs| -> Arc<dyn WorkspaceHost> {
        let shared: Arc<dyn WorkspaceBucket> = bucket.clone();
        Arc::new(StorageWorkspaceHost::with_clock(
            shared,
            &dirs.fresh(),
            pinned_clock(now.clone()),
        ))
    };
    let mut host = new_host(&mut dirs);
    let mut results = Vec::new();
    let mut objects = Vec::new();
    for step in &fixture.steps {
        now.store(step.at_ms + shift_ms, Ordering::SeqCst);
        if step.cold {
            host = new_host(&mut dirs);
        }
        let params: Map<String, Value> = step
            .params
            .iter()
            .map(|(k, v)| (k.clone(), resolve(v, &results)))
            .collect();
        let active = if step.no_host {
            None
        } else {
            Some(host.clone())
        };
        results.push(call(active, step.principal.clone(), &step.action, &params));
        objects.push(bucket.snapshot().keys().cloned().collect());
    }
    Replay {
        results,
        objects,
        bucket,
    }
}

/// The `.git` internals `tree` lists are the local git's own files (hook samples, sizes), not workspace data.
fn normalize(action: &str, data: Value) -> Value {
    if action != "tree" {
        return data;
    }
    let mut nodes: Vec<Value> = data["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| {
            let path = n["path"].as_str().unwrap_or("");
            path != ".git" && !path.starts_with(".git/")
        })
        .collect();
    nodes.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    serde_json::json!({ "nodes": nodes })
}

/// `None` when the replay equals the recording, else what differs.
fn differences(fixture: &Fixture, replay: &Replay) -> Option<String> {
    for (i, step) in fixture.steps.iter().enumerate() {
        let label = format!("{} step {i} ({})", fixture.case, step.action);
        match (&step.outcome, &replay.results[i]) {
            (Outcome::Ok { data }, Ok(actual)) => {
                let (expected, actual) = (
                    normalize(&step.action, data.clone()),
                    normalize(&step.action, actual.clone()),
                );
                if expected != actual {
                    return Some(format!(
                        "{label}: result\n  recorded {expected}\n  rust     {actual}"
                    ));
                }
            }
            (Outcome::Ok { .. }, Err(e)) => {
                return Some(format!("{label}: expected success, got {e:?}"))
            }
            (Outcome::Err { .. }, Ok(actual)) => {
                return Some(format!("{label}: expected an error, got {actual}"))
            }
            (Outcome::Err { code, message }, Err(e)) => {
                if &e.message != message || &e.code != code {
                    return Some(format!(
                        "{label}: error\n  recorded {code:?} {message}\n  rust     {:?} {}",
                        e.code, e.message
                    ));
                }
            }
        }
        if replay.objects[i] != step.objects {
            return Some(format!(
                "{label}: bucket objects\n  recorded {:?}\n  rust     {:?}",
                step.objects, replay.objects[i]
            ));
        }
    }
    let actual = replay.bucket.snapshot();
    for (name, object) in &fixture.bucket {
        match actual.get(name) {
            None => return Some(format!("{}: {name} is missing", fixture.case)),
            Some(bytes) if *bytes != object.bytes() => {
                return Some(match object {
                    Object::Text { text } => format!(
                        "{}: {name}\n  recorded {text}\n  rust     {}",
                        fixture.case,
                        String::from_utf8_lossy(bytes)
                    ),
                    Object::Bundle { .. } => {
                        format!("{}: {name} (git bundle bytes differ)", fixture.case)
                    }
                })
            }
            Some(_) => {}
        }
    }
    None
}

#[test]
fn every_recorded_sequence_replays_to_the_same_results_and_bucket() {
    let all = fixtures();
    assert!(
        all.len() >= 24,
        "expected the recorded workspace cases, found {}",
        all.len()
    );
    for fixture in &all {
        let run = replay(fixture, 0);
        if let Some(diff) = differences(fixture, &run) {
            panic!("{diff}");
        }
    }
}

#[test]
fn control_a_replay_under_a_different_clock_is_caught() {
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.case == "undo-redo-cycle")
        .expect("undo-redo-cycle fixture");
    assert!(
        differences(&fixture, &replay(&fixture, 0)).is_none(),
        "the unshifted replay must match first"
    );
    let drifted = replay(&fixture, 3_600_000);
    assert!(
        differences(&fixture, &drifted).is_some(),
        "another instant changes the commit ids and must fail the comparison"
    );
}

#[test]
fn control_a_bundle_that_differs_by_one_byte_is_caught() {
    let mut fixture = fixtures()
        .into_iter()
        .find(|f| f.case == "snapshot-then-cold-history")
        .expect("fixture");
    let run = replay(&fixture, 0);
    assert!(differences(&fixture, &run).is_none());
    let base = fixture
        .bucket
        .keys()
        .find(|n| n.ends_with("/history/base.bundle"))
        .cloned()
        .expect("base bundle");
    let mut bytes = fixture.bucket[&base].bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fixture.bucket.insert(
        base,
        Object::Bundle {
            base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    );
    assert!(differences(&fixture, &run).is_some());
}

fn changed_history(step: &Step) -> bool {
    matches!(
        step.action.as_str(),
        "snapshot" | "restore" | "undo" | "redo"
    ) && matches!(&step.outcome, Outcome::Ok { data } if data["changed"] == true || data["committed"] == true)
}

fn seeded_bucket(fixture: &Fixture) -> Arc<MemoryBucket> {
    let bucket = Arc::new(MemoryBucket::new());
    for (name, object) in &fixture.bucket {
        bucket.write(name, &object.bytes()).expect("seed");
    }
    bucket
}

#[test]
fn the_rust_twin_reads_the_history_the_ts_host_left_in_the_bucket() {
    let mut checked = 0;
    for fixture in fixtures() {
        let Some(last) = fixture.steps.iter().rposition(|s| s.action == "history") else {
            continue;
        };
        if fixture.steps[last + 1..].iter().any(changed_history) {
            continue;
        }
        let Outcome::Ok { data } = &fixture.steps[last].outcome else {
            continue;
        };
        let mut dirs = CacheDirs(Vec::new());
        let shared: Arc<dyn WorkspaceBucket> = seeded_bucket(&fixture);
        let now = Arc::new(AtomicI64::new(fixture.steps[last].at_ms));
        let host: Arc<dyn WorkspaceHost> = Arc::new(StorageWorkspaceHost::with_clock(
            shared,
            &dirs.fresh(),
            pinned_clock(now),
        ));
        let params = fixture.steps[last].params.clone();
        let read = call(
            Some(host),
            fixture.steps[last].principal.clone(),
            "history",
            &params,
        )
        .unwrap_or_else(|e| panic!("{}: {e}", fixture.case));
        assert_eq!(
            &read, data,
            "{}: history read from the TS bucket",
            fixture.case
        );
        checked += 1;
    }
    assert!(
        checked >= 8,
        "only {checked} cases carry a final history read"
    );
}

#[test]
fn control_a_corrupted_bundle_is_refused_not_read_as_empty_history() {
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.case == "snapshot-then-cold-history")
        .expect("fixture");
    let bucket = seeded_bucket(&fixture);
    let base = bucket
        .snapshot()
        .keys()
        .find(|n| n.ends_with("/history/base.bundle"))
        .cloned()
        .expect("base bundle");
    bucket.write(&base, b"not a git bundle").expect("corrupt");
    let mut dirs = CacheDirs(Vec::new());
    let shared: Arc<dyn WorkspaceBucket> = bucket;
    let host: Arc<dyn WorkspaceHost> = Arc::new(StorageWorkspaceHost::new(shared, &dirs.fresh()));
    let mut params = Map::new();
    params.insert("workspace".into(), Value::String("app-1".into()));
    let read = call(Some(host), Some("user-1".into()), "history", &params);
    assert!(
        read.is_err(),
        "a corrupted archive must fail loudly, got {read:?}"
    );
}

#[test]
fn control_a_second_principal_never_reads_the_first_ones_bucket_objects() {
    let fixture = fixtures()
        .into_iter()
        .find(|f| f.case == "two-callers-never-share")
        .expect("fixture");
    let run = replay(&fixture, 0);
    let names: Vec<String> = run.bucket.snapshot().keys().cloned().collect();
    assert!(names.iter().any(|n| n.starts_with("workspaces/user-1/")));
    assert!(names.iter().any(|n| n.starts_with("workspaces/user-2/")));
    let mine = run.bucket.snapshot();
    assert!(!mine
        .iter()
        .any(|(n, _)| n.starts_with("workspaces/user-2/") && n.ends_with("mine.lolo")));
}

#[test]
fn local_host_keeps_files_and_history_under_the_project_root() {
    let mut dirs = CacheDirs(Vec::new());
    let root = dirs.fresh();
    let host: Arc<dyn WorkspaceHost> = Arc::new(workspace::LocalWorkspaceHost::new(&root));
    assert_eq!(host.root(), Path::new(&root).join("workspaces"));
    let params = |pairs: &[(&str, &str)]| -> Map<String, Value> {
        let mut m: HashMap<String, Value> = HashMap::new();
        m.insert("workspace".into(), Value::String("app-1".into()));
        for (k, v) in pairs {
            m.insert((*k).into(), Value::String((*v).into()));
        }
        m.into_iter().collect()
    };
    let who = Some("user-1".to_string());
    call(
        Some(host.clone()),
        who.clone(),
        "writeFile",
        &params(&[("path", "a.lolo"), ("content", "one")]),
    )
    .expect("write");
    let snap = call(
        Some(host.clone()),
        who.clone(),
        "snapshot",
        &params(&[("reason", "first")]),
    )
    .expect("snapshot");
    assert_eq!(snap["committed"], true);
    let read = call(
        Some(host.clone()),
        who,
        "readFile",
        &params(&[("path", "a.lolo")]),
    )
    .expect("read");
    assert_eq!(read["content"], "one");
    let none = call(Some(host), None, "tree", &params(&[]));
    assert_eq!(none.unwrap_err().code.as_deref(), Some("AUTH_ERROR"));
}

#[test]
fn fixtures_hold_no_credentials() {
    for fixture in fixtures() {
        for step in &fixture.steps {
            let text = serde_json::to_string(&step.params).unwrap_or_default();
            assert!(!text.contains("test-key"), "{}", fixture.case);
        }
    }
}
