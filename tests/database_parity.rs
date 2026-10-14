//! Twin parity: every session `@almadar/integrations` recorded for the `database` provider
//! (`fixtures/database/*.json`, written by `__tests__/database-recorded-exchanges.test.ts`) is replayed against
//! the Rust provider through a connector that asserts the same pools are opened and the same queries sent, and
//! answers with the recorded reply. The SQL guard replays on a vector list of TS verdicts.

mod infra_support;

use orbital_integrations::CallContext;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use infra_support::{assert_no_secrets, check_outcome, fixture, fixtures, Fixture, Outcome};
use orbital_integrations::database::{
    self, assert_read_only_select, Connection, Connector, DatabaseProvider, DriverError,
    QueryResult,
};
use orbital_integrations::{Credentials, Integrations};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize, Clone)]
struct DbStep {
    driver: Driver,
    replies: Vec<Reply>,
}

#[derive(Deserialize, Clone)]
struct Driver {
    connects: Vec<Connect>,
    queries: Vec<Query>,
}

#[derive(Deserialize, Clone, PartialEq, Debug)]
struct Connect {
    #[serde(rename = "connectionString")]
    connection_string: String,
    #[serde(rename = "statementTimeoutMs")]
    statement_timeout_ms: Option<f64>,
}

#[derive(Deserialize, Clone, PartialEq, Debug)]
struct Query {
    sql: String,
    params: Vec<Value>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum Reply {
    Rows {
        rows: Vec<Map<String, Value>>,
        #[serde(rename = "rowCount")]
        row_count: Option<u64>,
    },
    Error {
        error: ErrorReply,
    },
}

#[derive(Deserialize, Clone)]
struct ErrorReply {
    message: String,
    code: Option<String>,
}

#[derive(Default)]
struct Script {
    case: String,
    connects: Vec<Connect>,
    queries: Vec<Query>,
    replies: Vec<Reply>,
}

struct ScriptedConnector(Arc<Mutex<Script>>);
struct ScriptedConnection(Arc<Mutex<Script>>);

impl Connector for ScriptedConnector {
    fn connect(
        &self,
        connection_string: &str,
        statement_timeout_ms: f64,
    ) -> Result<Arc<dyn Connection>, DriverError> {
        self.0.lock().expect("script").connects.push(Connect {
            connection_string: connection_string.to_string(),
            statement_timeout_ms: Some(statement_timeout_ms),
        });
        Ok(Arc::new(ScriptedConnection(self.0.clone())))
    }
}

impl Connection for ScriptedConnection {
    fn query(&self, sql: &str, params: &[Value]) -> Result<QueryResult, DriverError> {
        let mut script = self.0.lock().expect("script");
        script.queries.push(Query {
            sql: sql.to_string(),
            params: params.to_vec(),
        });
        assert!(
            !script.replies.is_empty(),
            "{}: the provider sent an unrecorded query: {sql}",
            script.case
        );
        match script.replies.remove(0) {
            Reply::Rows { rows, row_count } => Ok(QueryResult { rows, row_count }),
            Reply::Error { error } => Err(DriverError {
                message: error.message,
                code: error.code,
            }),
        }
    }
}

fn play(fixture: &Fixture) {
    #[derive(Deserialize)]
    struct Setup {
        #[serde(rename = "processEnv")]
        process_env: HashMap<String, String>,
    }
    let setup: Setup = serde_json::from_value(fixture.setup.clone()).expect("setup");
    let mut env = fixture.env.clone();
    env.extend(setup.process_env);
    let creds = Credentials::from_env(env);
    let script = Arc::new(Mutex::new(Script::default()));
    let mut integrations = Integrations::new(&creds).expect("mode");
    database::register(
        &mut integrations,
        &creds,
        Arc::new(ScriptedConnector(script.clone())),
    );
    let raw: Value = serde_json::to_value(&fixture.setup).expect("value");
    let _ = raw;
    let steps = recorded_steps(&fixture.case);
    for (index, step) in fixture.steps.iter().enumerate() {
        let label = format!("{} step {index}", fixture.case);
        let recorded = &steps[index];
        {
            let mut s = script.lock().expect("script");
            s.case = label.clone();
            s.connects.clear();
            s.queries.clear();
            s.replies = recorded.replies.clone();
        }
        let result = integrations.call_json(
            database::SERVICE,
            &step.action,
            &step.params,
            &CallContext::anonymous(),
        );
        check_outcome(&label, &step.outcome, result);
        let s = script.lock().expect("script");
        let connects: Vec<Connect> = s.connects.clone();
        assert_eq!(
            connects.len(),
            recorded.driver.connects.len(),
            "{label}: pools opened"
        );
        for (got, want) in connects.iter().zip(&recorded.driver.connects) {
            assert_eq!(
                got.connection_string, want.connection_string,
                "{label}: connection string"
            );
            assert_eq!(
                got.statement_timeout_ms, want.statement_timeout_ms,
                "{label}: statement timeout"
            );
        }
        assert_eq!(s.queries, recorded.driver.queries, "{label}: queries sent");
        assert!(
            s.replies.is_empty(),
            "{label}: recorded replies never asked for"
        );
    }
}

fn recorded_steps(case: &str) -> Vec<DbStep> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("fixtures/database/{case}.json"));
    #[derive(Deserialize)]
    struct File {
        steps: Vec<DbStep>,
    }
    let file: File =
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("steps");
    file.steps
}

#[test]
fn every_recorded_session_replays_to_the_same_results() {
    let all: Vec<Fixture> = fixtures("database")
        .into_iter()
        .filter(|f| !f.steps.is_empty())
        .collect();
    assert!(
        all.len() >= 7,
        "expected the recorded database sessions, found {}",
        all.len()
    );
    for fixture in &all {
        play(fixture);
    }
}

#[derive(Deserialize)]
struct GuardFile {
    vectors: Vec<GuardVector>,
    #[serde(rename = "constructorErrors")]
    constructor_errors: Vec<ConstructorError>,
}

#[derive(Deserialize)]
struct GuardVector {
    sql: String,
    ok: bool,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ConstructorError {
    value: String,
    message: String,
}

fn guard_file() -> GuardFile {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/database/guard-and-constructor.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("guard fixture"))
        .expect("guard vectors")
}

#[test]
fn the_sql_guard_agrees_with_every_recorded_verdict() {
    let file = guard_file();
    assert!(file.vectors.len() >= 60);
    for v in &file.vectors {
        let got = assert_read_only_select(&v.sql);
        assert_eq!(
            (got.ok, got.reason),
            (v.ok, v.reason.clone()),
            "{:?}",
            v.sql
        );
    }
}

#[test]
fn a_bad_statement_timeout_is_the_constructor_error() {
    for case in guard_file().constructor_errors {
        let creds = Credentials::from_env(
            [(
                "DATABASE_STATEMENT_TIMEOUT_MS".to_string(),
                case.value.clone(),
            )]
            .into_iter()
            .collect(),
        );
        let err = DatabaseProvider::new(
            &creds,
            Arc::new(ScriptedConnector(Arc::new(Mutex::new(Script::default())))),
        )
        .err()
        .expect("constructor error");
        assert_eq!(err.message, case.message, "{}", case.value);
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture("database", "queries-and-pooling");
    if let Outcome::Ok { data } = &mut drifted.steps[0].outcome {
        data["rowCount"] = Value::from(99);
    }
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(
        outcome.is_err(),
        "a different row count must fail the replay"
    );
}

#[test]
fn control_the_guard_verdict_flips_with_the_sql() {
    assert!(assert_read_only_select("SELECT 1").ok);
    assert!(!assert_read_only_select("SELECT 1 FOR UPDATE").ok);
}

#[test]
fn fixtures_hold_no_real_secrets() {
    let all: Vec<Fixture> = fixtures("database")
        .into_iter()
        .filter(|f| !f.steps.is_empty())
        .collect();
    assert_no_secrets(&all);
    for fixture in &all {
        for (name, value) in fixture.setup["processEnv"].as_object().expect("env") {
            if value.as_str().is_some_and(|v| v.starts_with("postgres://")) {
                assert!(
                    value.as_str().expect("v").contains(":test-key@"),
                    "{name} in {}",
                    fixture.case
                );
            }
        }
    }
}
