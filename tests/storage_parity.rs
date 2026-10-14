//! Twin parity: every session `@almadar/integrations` recorded for the `storage` provider
//! (`fixtures/storage/*.json`, written by `__tests__/storage-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider: the in-memory backend under the recorded clock, the S3 backend through a transport that
//! asserts the same requests (method, URL, content type, an authorization header) and returns the recorded reply.

mod infra_support;

use orbital_integrations::CallContext;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use infra_support::{
    assert_no_secrets, fixture, fixtures, replay, Fixture, ReplayTransport, ScriptedClock,
    ScriptedRand,
};
use orbital_integrations::infra::SystemRand;
use orbital_integrations::storage::{self, StorageProvider};
use orbital_integrations::{Credentials, Integrations};

struct Rig {
    waits: Arc<Mutex<Vec<Duration>>>,
}

fn play(fixture: &Fixture) -> Rig {
    let creds = Credentials::from_env(fixture.env.clone());
    let clock = ScriptedClock::new();
    let rand = ScriptedRand::new();
    let transport = Arc::new(ReplayTransport::new());
    let waits = Arc::new(Mutex::new(Vec::new()));
    let recorded = waits.clone();
    let provider = StorageProvider::new(&creds, transport.clone())
        .expect("provider")
        .with_clock(clock.clock())
        .with_retry(
            Arc::new(move |d| recorded.lock().expect("waits").push(d)),
            Arc::new(SystemRand::new()),
        );
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(storage::SERVICE, Arc::new(provider));
    replay(
        fixture,
        storage::SERVICE,
        &integrations,
        &clock,
        &rand,
        Some(&transport),
    );
    Rig { waits }
}

#[test]
fn every_recorded_session_replays_to_the_same_results() {
    let all = fixtures("storage");
    assert!(
        all.len() >= 12,
        "expected the recorded storage sessions, found {}",
        all.len()
    );
    for fixture in &all {
        play(fixture);
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture("storage", "s3-object-round-trip");
    drifted.steps[0].exchanges[0].request.url = drifted.steps[0].exchanges[0]
        .request
        .url
        .replace("/media/a/one.txt", "/media/a/other.txt");
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(
        outcome.is_err(),
        "a different request url must fail the replay"
    );
}

#[test]
fn control_a_different_instant_changes_a_presigned_url() {
    let mut drifted = fixture("storage", "s3-signed-urls");
    drifted.steps[0].clock_ms += 1_000;
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(outcome.is_err(), "the signature depends on the instant");
}

#[test]
fn the_sdk_retry_policy_waits_between_attempts() {
    let rig = play(&fixture("storage", "s3-errors"));
    // flaky: 3 attempts (2 waits), throttled: 2 attempts (1 wait), gateway: 3 attempts (2 waits)
    assert_eq!(rig.waits.lock().expect("waits").len(), 5);
}

#[test]
fn storage_without_credentials_in_production_refuses_the_in_memory_fallback() {
    let creds = Credentials::from_env(
        [("NODE_ENV".to_string(), "production".to_string())]
            .into_iter()
            .collect(),
    );
    let mut integrations = Integrations::new(&creds).expect("mode");
    storage::register(&mut integrations, &creds, Arc::new(ReplayTransport::new()));
    let err = integrations
        .call_json(
            "storage",
            "list",
            &serde_json::from_str(r#"{"bucket":"b"}"#).expect("params"),
            &CallContext::anonymous(),
        )
        .unwrap_err();
    assert!(
        err.message
            .starts_with("Storage credentials missing in production"),
        "{err:?}"
    );

    let dev = Credentials::default();
    let mut integrations = Integrations::new(&dev).expect("mode");
    storage::register(&mut integrations, &dev, Arc::new(ReplayTransport::new()));
    assert!(integrations
        .call_json(
            "storage",
            "list",
            &serde_json::from_str(r#"{"bucket":"b"}"#).expect("params"),
            &CallContext::anonymous()
        )
        .is_ok());
}

#[test]
fn fixtures_hold_no_real_secrets() {
    let all = fixtures("storage");
    assert_no_secrets(&all);
    for fixture in &all {
        if let Some(secret) = fixture.env.get("STORAGE_SECRET_ACCESS_KEY") {
            assert_eq!(secret, "test-key", "{}", fixture.case);
        }
    }
}
