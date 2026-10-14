//! The native Postgres connector against a real server, compared with what `node-postgres` returned for the same
//! queries (`fixtures/database/live-pg-vectors.json`, recorded with `ALMADAR_TEST_PG_URL` set and `TZ=UTC`).
//! Run with a server up: `ALMADAR_TEST_PG_URL=postgres://... cargo test -p orbital-integrations --test postgres_live -- --ignored`.
#![cfg(feature = "postgres")]

use orbital_integrations::database::postgres::PostgresConnector;
use orbital_integrations::database::Connector;
use serde::Deserialize;
use serde_json::{json, Map, Value};

#[derive(Deserialize)]
struct Vectors {
    queries: Vec<Query>,
    results: Vec<Value>,
}

#[derive(Deserialize)]
struct Query {
    sql: String,
    params: Vec<Value>,
}

#[test]
#[ignore = "needs ALMADAR_TEST_PG_URL pointing at a Postgres server"]
fn every_live_query_returns_what_node_postgres_returned() {
    let url = std::env::var("ALMADAR_TEST_PG_URL").expect("ALMADAR_TEST_PG_URL");
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/database/live-pg-vectors.json");
    let vectors: Vectors =
        serde_json::from_str(&std::fs::read_to_string(path).expect("vectors")).expect("parse");
    let connection = PostgresConnector.connect(&url, 10_000.0).expect("connect");
    for (query, expected) in vectors.queries.iter().zip(&vectors.results) {
        let got = match connection.query(&query.sql, &query.params) {
            Ok(result) => {
                json!({ "rows": result.rows.into_iter().map(Value::Object).collect::<Vec<_>>(), "rowCount": result.row_count })
            }
            Err(error) => json!({ "error": { "message": error.message, "code": error.code } }),
        };
        assert_eq!(&got, expected, "{}", query.sql);
    }
    let _ = Map::<String, Value>::new();
}

#[test]
#[ignore = "needs ALMADAR_TEST_PG_URL pointing at a Postgres server"]
fn a_slow_query_is_cancelled_by_the_statement_timeout() {
    let url = std::env::var("ALMADAR_TEST_PG_URL").expect("ALMADAR_TEST_PG_URL");
    let connection = PostgresConnector.connect(&url, 300.0).expect("connect");
    let error = connection
        .query("SELECT pg_sleep(5)", &[])
        .expect_err("cancelled");
    assert_eq!(error.code.as_deref(), Some("57014"));
}
