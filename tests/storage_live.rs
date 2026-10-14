//! The S3 backend against a real S3-compatible server (MinIO): signed uploads, listings and deletes, and
//! presigned URLs fetched back over HTTP. Run with a server up and a `media` bucket:
//! `ALMADAR_TEST_S3_ENDPOINT=http://127.0.0.1:9000 ALMADAR_TEST_S3_KEY=<id> ALMADAR_TEST_S3_SECRET=<secret> cargo test -p orbital-integrations --test storage_live -- --ignored`.
#![cfg(feature = "http")]

use orbital_integrations::CallContext;
use std::collections::HashMap;
use std::sync::Arc;

use orbital_integrations::storage::StorageProvider;
use orbital_integrations::{Credentials, HttpRequest, Integrations, Transport, UreqTransport};
use serde_json::{json, Map, Value};

fn rig() -> Option<(Integrations, Arc<UreqTransport>)> {
    let endpoint = std::env::var("ALMADAR_TEST_S3_ENDPOINT").ok()?;
    let env: HashMap<String, String> = [
        ("STORAGE_ENDPOINT", endpoint),
        (
            "STORAGE_ACCESS_KEY_ID",
            std::env::var("ALMADAR_TEST_S3_KEY").ok()?,
        ),
        (
            "STORAGE_SECRET_ACCESS_KEY",
            std::env::var("ALMADAR_TEST_S3_SECRET").ok()?,
        ),
        ("STORAGE_BUCKET", "media".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let creds = Credentials::from_env(env);
    let transport = Arc::new(UreqTransport::new());
    let provider = StorageProvider::new(&creds, transport.clone()).expect("provider");
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register("storage", Arc::new(provider));
    Some((integrations, transport))
}

fn call(i: &Integrations, action: &str, params: Value) -> Result<Value, String> {
    let map: Map<String, Value> = params.as_object().expect("object").clone();
    i.call_json("storage", action, &map, &CallContext::anonymous())
        .map_err(|e| e.message)
}

#[test]
#[ignore = "needs ALMADAR_TEST_S3_ENDPOINT / _KEY / _SECRET pointing at an S3-compatible server"]
fn objects_round_trip_through_signed_requests_and_presigned_urls() {
    let (storage, http) = rig().expect("ALMADAR_TEST_S3_* variables");
    let key = "live/dir with space/\u{e9}\u{e8}+plus.txt";
    let uploaded = call(&storage, "upload", json!({ "bucket": "media", "key": key, "content": "hello live", "contentType": "text/plain" })).expect("upload");
    assert_eq!(uploaded["size"], 10);

    let downloaded = call(
        &storage,
        "download",
        json!({ "bucket": "media", "key": key }),
    )
    .expect("download");
    assert_eq!(downloaded["contentType"], "text/plain");
    assert_eq!(downloaded["content"], "aGVsbG8gbGl2ZQ==");

    let listed = call(
        &storage,
        "list",
        json!({ "bucket": "media", "prefix": "live/" }),
    )
    .expect("list");
    assert_eq!(listed["keys"][0]["key"], key);
    assert_eq!(listed["keys"][0]["size"], 10);
    assert_eq!(listed["truncated"], false);

    let signed = call(
        &storage,
        "getSignedUrl",
        json!({ "bucket": "media", "key": key, "expiresIn": 120 }),
    )
    .expect("sign");
    let fetched = http
        .send(&HttpRequest {
            method: "GET".into(),
            url: signed["url"].as_str().expect("url").into(),
            headers: vec![],
            body: Vec::new(),
            timeout: None,
        })
        .expect("fetch");
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.text(), "hello live");

    let put_url = call(
        &storage,
        "getSignedUrl",
        json!({ "bucket": "media", "key": "live/put-by-url.txt", "operation": "put" }),
    )
    .expect("sign put");
    let put = http
        .send(&HttpRequest {
            method: "PUT".into(),
            url: put_url["url"].as_str().expect("url").into(),
            headers: vec![],
            body: b"via presigned put".to_vec(),
            timeout: None,
        })
        .expect("put");
    assert!(put.is_success(), "{} {}", put.status, put.text());
    let again = call(
        &storage,
        "download",
        json!({ "bucket": "media", "key": "live/put-by-url.txt" }),
    )
    .expect("download again");
    assert_eq!(again["size"], 17);

    assert_eq!(
        call(&storage, "delete", json!({ "bucket": "media", "key": key })).expect("delete")
            ["deleted"],
        true
    );
    call(
        &storage,
        "delete",
        json!({ "bucket": "media", "key": "live/put-by-url.txt" }),
    )
    .expect("delete 2");
    let missing = call(
        &storage,
        "download",
        json!({ "bucket": "media", "key": key }),
    )
    .expect_err("gone");
    assert_eq!(missing, "The specified key does not exist.");
}
