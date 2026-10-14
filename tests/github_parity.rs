//! Twin parity: every exchange `@almadar/integrations` recorded for the `github` provider
//! (`fixtures/github/*.json`, written by `__tests__/github-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider through a mock transport. It must send the same requests and produce the same result or error.
//! Cases with `setup: "git-repo"` run the git CLI actions against a throwaway repository, as the recording did.

use orbital_integrations::CallContext;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use orbital_integrations::github::{self, GithubProvider};
use orbital_integrations::{
    Credentials, HttpRequest, HttpResponse, IntegrationError, Integrations, Transport,
    TransportError,
};
use serde::Deserialize;
use serde_json::{Map, Value};

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
    #[serde(default)]
    setup: Option<String>,
    exchanges: Vec<Exchange>,
    outcome: Outcome,
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
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/github");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| {
            panic!("fixtures/github is missing ({e}); record it with ALMADAR_RECORD_FIXTURES")
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
        assert_eq!(request.url, expected.request.url, "{case}: url");
        assert_eq!(
            semantic(&request.headers),
            expected.request.headers,
            "{case}: headers"
        );
        if expected.request.body.is_empty() {
            assert!(request.body.is_empty(), "{case}: request body");
        } else {
            let sent: Value = serde_json::from_str(&request.text()).unwrap_or_else(|e| {
                panic!("{case}: sent a non-JSON body ({e}): {}", request.text())
            });
            let recorded: Value =
                serde_json::from_str(&expected.request.body).expect("recorded body is JSON");
            assert_eq!(sent, recorded, "{case}: request body");
        }
        Ok(HttpResponse {
            status: expected.response.status,
            headers: expected.response.headers.into_iter().collect(),
            body: expected.response.body.into_bytes(),
        })
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(status.status.success(), "git {args:?} failed");
}

/// One commit, a bare `origin` and an untracked `note.txt`: the repository the recording ran in.
fn make_repo(label: &str) -> (PathBuf, PathBuf) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("orb-github-{label}-{}-{nanos}", std::process::id()));
    let origin = root.join("origin.git");
    let work = root.join("work");
    std::fs::create_dir_all(&root).expect("scratch dir");
    git(&root, &["init", "--bare", "-q", "origin.git"]);
    git(&root, &["init", "-q", "-b", "main", "work"]);
    git(&work, &["config", "user.name", "Fixture"]);
    git(&work, &["config", "user.email", "fixture@example.com"]);
    git(&work, &["config", "commit.gpgsign", "false"]);
    std::fs::write(work.join("README.md"), "seed\n").expect("seed file");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "seed"]);
    git(
        &work,
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().expect("utf8 path"),
        ],
    );
    std::fs::write(work.join("note.txt"), "note\n").expect("note file");
    (root, work)
}

fn run(fixture: &Fixture) -> Result<Value, IntegrationError> {
    let scratch = (fixture.setup.as_deref() == Some("git-repo")).then(|| make_repo(&fixture.case));
    let env: HashMap<String, String> = fixture
        .env
        .iter()
        .map(|(k, v)| {
            let value = match &scratch {
                Some((_, work)) => v.replace("{workdir}", work.to_str().expect("utf8 path")),
                None => v.clone(),
            };
            (k.clone(), value)
        })
        .collect();
    let creds = Credentials::from_env(env);
    let transport = Arc::new(ReplayTransport {
        case: fixture.case.clone(),
        script: Mutex::new(fixture.exchanges.iter().cloned().collect()),
    });
    let mut integrations = Integrations::new(&creds).expect("mode");
    let dynamic: Arc<dyn Transport> = transport.clone();
    match GithubProvider::new(&creds, dynamic) {
        Ok(provider) => integrations.register(github::SERVICE, Arc::new(provider)),
        Err(e) => integrations.register_failed(github::SERVICE, e),
    }
    let result = integrations.call_json(
        github::SERVICE,
        &fixture.action,
        &fixture.params,
        &CallContext::anonymous(),
    );
    let unused = transport.script.lock().expect("script lock").len();
    if let Some((root, _)) = scratch {
        let _ = std::fs::remove_dir_all(root);
    }
    assert_eq!(
        unused, 0,
        "{}: recorded requests the provider never sent",
        fixture.case
    );
    result
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures();
    assert!(
        all.len() >= 40,
        "expected the recorded github cases, found {}",
        all.len()
    );
    for fixture in &all {
        match (&fixture.outcome, run(fixture)) {
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
fn every_action_has_a_recorded_success_and_failure() {
    let all = fixtures();
    for action in [
        "createPR",
        "getPRComments",
        "listIssues",
        "getIssue",
        "getFile",
        "listCommits",
        "createIssue",
        "cloneRepo",
        "createBranch",
        "commit",
        "push",
    ] {
        let of_action: Vec<&Fixture> = all.iter().filter(|f| f.action == action).collect();
        assert!(!of_action.is_empty(), "{action}: no fixture");
    }
    for action in [
        "createPR",
        "getPRComments",
        "listIssues",
        "getIssue",
        "getFile",
        "listCommits",
        "createIssue",
    ] {
        assert!(
            all.iter()
                .any(|f| f.action == action && matches!(f.outcome, Outcome::Ok { .. })),
            "{action}: no success fixture"
        );
        assert!(
            all.iter()
                .any(|f| f.action == action && matches!(f.outcome, Outcome::Err { .. })),
            "{action}: no failure fixture"
        );
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixtures()
        .into_iter()
        .find(|f| f.case == "create-pr")
        .expect("create-pr fixture");
    drifted.exchanges[0].request.body = drifted.exchanges[0]
        .request
        .body
        .replace("\"draft\":false", "\"draft\":true");
    let outcome = std::panic::catch_unwind(|| run(&drifted));
    assert!(
        outcome.is_err(),
        "a different request body must fail the replay"
    );
}

#[test]
fn control_a_different_url_is_caught() {
    let mut drifted = fixtures()
        .into_iter()
        .find(|f| f.case == "list-issues-options")
        .expect("fixture");
    drifted.exchanges[0].request.url = drifted.exchanges[0]
        .request
        .url
        .replace("per_page=100", "per_page=500");
    assert!(std::panic::catch_unwind(|| run(&drifted)).is_err());
}

#[test]
fn fixtures_hold_no_real_secrets() {
    for fixture in fixtures() {
        for (name, value) in &fixture.env {
            if name.ends_with("_TOKEN") {
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

struct Capture(
    Mutex<Vec<HttpRequest>>,
    Result<HttpResponse, TransportError>,
);

impl Transport for Capture {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        self.0.lock().expect("capture").push(request.clone());
        self.1.clone()
    }
}

fn creds(pairs: &[(&str, &str)]) -> Credentials {
    Credentials::from_env(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

fn params(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

#[test]
fn every_api_request_carries_the_bearer_token_and_the_api_version() {
    let capture = Arc::new(Capture(
        Mutex::new(Vec::new()),
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: b"[]".to_vec(),
        }),
    ));
    let c = creds(&[("GITHUB_TOKEN", "tok-123")]);
    let mut integrations = Integrations::new(&c).expect("mode");
    github::register(&mut integrations, &c, capture.clone());
    integrations
        .call_json(
            github::SERVICE,
            "listCommits",
            &params(serde_json::json!({ "owner": "a", "repo": "b" })),
            &CallContext::anonymous(),
        )
        .expect("ok");
    let sent = capture.0.lock().expect("capture");
    let header = |name: &str| {
        sent[0]
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header("authorization").as_deref(), Some("Bearer tok-123"));
    assert_eq!(
        header("x-github-api-version").as_deref(),
        Some("2022-11-28")
    );
    assert_eq!(sent[0].url, "https://api.github.com/repos/a/b/commits");
}

#[test]
fn a_dead_connection_reads_like_node_fetch() {
    let capture = Arc::new(Capture(
        Mutex::new(Vec::new()),
        Err(TransportError::Failed("connection refused".to_string())),
    ));
    let c = creds(&[("GITHUB_TOKEN", "t")]);
    let mut integrations = Integrations::new(&c).expect("mode");
    github::register(&mut integrations, &c, capture);
    let error = integrations
        .call_json(
            github::SERVICE,
            "listCommits",
            &params(serde_json::json!({ "owner": "a", "repo": "b" })),
            &CallContext::anonymous(),
        )
        .expect_err("fails");
    assert_eq!(error.code.as_deref(), Some("NETWORK_ERROR"));
    assert_eq!(error.message, "GitHub API request failed: fetch failed");
}

#[test]
fn without_a_token_the_service_is_unconfigured() {
    let capture = Arc::new(Capture(
        Mutex::new(Vec::new()),
        Err(TransportError::TimedOut),
    ));
    let c = creds(&[]);
    let mut integrations = Integrations::new(&c).expect("mode");
    github::register(&mut integrations, &c, capture);
    let error = integrations
        .call_json(
            github::SERVICE,
            "listCommits",
            &params(serde_json::json!({ "owner": "a", "repo": "b" })),
            &CallContext::anonymous(),
        )
        .expect_err("unconfigured");
    assert_eq!(error.code.as_deref(), Some("NOT_CONFIGURED"));
    assert!(error.message.contains("github"), "{}", error.message);
}

#[test]
fn clone_checks_out_a_local_repository() {
    let (root, work) = make_repo("clone");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "note"]);
    let target = root.join("cloned");
    let source_url = format!("file://{}", work.to_str().expect("utf8 path"));
    let c = creds(&[
        ("GITHUB_TOKEN", "tok-secret"),
        ("GITHUB_OWNER", "o"),
        ("GITHUB_REPO", "r"),
    ]);
    let capture = Arc::new(Capture(
        Mutex::new(Vec::new()),
        Err(TransportError::TimedOut),
    ));
    let mut integrations = Integrations::new(&c).expect("mode");
    github::register(&mut integrations, &c, capture);
    let out = integrations
        .call_json(
            github::SERVICE,
            "cloneRepo",
            &params(serde_json::json!({ "repoUrl": source_url, "targetDir": target.to_str().expect("utf8 path") })), &CallContext::anonymous())
        .expect("clone succeeds");
    assert_eq!(
        out["message"],
        format!("Successfully cloned {source_url} to {}", target.display())
    );
    assert!(target.join("note.txt").exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_failed_clone_never_prints_the_token() {
    let c = creds(&[
        ("GITHUB_TOKEN", "tok-secret"),
        ("GITHUB_OWNER", "o"),
        ("GITHUB_REPO", "r"),
    ]);
    let capture = Arc::new(Capture(
        Mutex::new(Vec::new()),
        Err(TransportError::TimedOut),
    ));
    let mut integrations = Integrations::new(&c).expect("mode");
    github::register(&mut integrations, &c, capture);
    let target = std::env::temp_dir().join(format!("orb-github-noclone-{}", std::process::id()));
    let error = integrations
        .call_json(
            github::SERVICE,
            "cloneRepo",
            &params(serde_json::json!({ "repoUrl": "http://127.0.0.1:1/o/r", "targetDir": target.to_str().expect("utf8 path") })), &CallContext::anonymous())
        .expect_err("nothing listens on port 1");
    assert_eq!(error.code.as_deref(), Some("SERVICE_ERROR"));
    assert!(
        error
            .message
            .starts_with("Failed to clone repository: Git command failed: clone"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("tok-secret"), "{}", error.message);
}

#[test]
fn clone_adopts_owner_and_repo_from_the_url_so_the_api_actions_work_after() {
    let c = creds(&[("GITHUB_TOKEN", "t")]);
    let capture = Arc::new(Capture(
        Mutex::new(Vec::new()),
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: b"[]".to_vec(),
        }),
    ));
    let mut integrations = Integrations::new(&c).expect("mode");
    github::register(&mut integrations, &c, capture.clone());
    let bad_target = std::env::temp_dir().join("orb-github-adopt-never");
    let _ = integrations.call_json(
        github::SERVICE,
        "cloneRepo",
        &params(serde_json::json!({ "repoUrl": "http://127.0.0.1:1/adopted/thing.git", "targetDir": bad_target.to_str().expect("utf8 path") })), &CallContext::anonymous());
    integrations
        .call_json(
            github::SERVICE,
            "getPRComments",
            &params(serde_json::json!({ "prNumber": 3 })),
            &CallContext::anonymous(),
        )
        .expect("comments");
    let sent = capture.0.lock().expect("capture");
    assert_eq!(
        sent[0].url,
        "https://api.github.com/repos/adopted/thing/pulls/3/comments"
    );
}
