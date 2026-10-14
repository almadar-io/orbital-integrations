//! Twin parity for the `youtube` provider: every case `__tests__/youtube-recorded-exchanges.test.ts`
//! recorded (`fixtures/youtube/*.json`) replays to the same result or error, retries included.

mod l13d_common;

use orbital_integrations::CallContext;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use l13d_common::*;
use orbital_integrations::youtube::{self, YouTubeProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    _fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    let provider = YouTubeProvider::new(creds, transport)?;
    Ok(Arc::new(provider.with_sleeper(Arc::new(|_| {}))))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(youtube::SERVICE, 31, &[], build);
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(youtube::SERVICE, "search");
    drifted.exchanges[0].request.url = drifted.exchanges[0]
        .request
        .url
        .replace("maxResults=10", "maxResults=11");
    let outcome = std::panic::catch_unwind(|| run(&drifted, &[], build));
    assert!(outcome.is_err(), "a different url must fail the replay");
}

#[test]
fn the_recorded_retries_wait_the_sdk_backoff() {
    let f = fixture(
        youtube::SERVICE,
        "search-server-error-exhausts-the-sdk-retries",
    );
    let waits = Arc::new(Mutex::new(Vec::new()));
    let seen = waits.clone();
    let creds = credentials(&f);
    let replay = Replay::new(&f, &[]);
    let transport: Arc<dyn Transport> = replay.clone();
    let provider = YouTubeProvider::new(&creds, transport)
        .expect("configured")
        .with_sleeper(Arc::new(move |d| seen.lock().expect("waits").push(d)));
    let err = provider
        .call("search", &f.params, &CallContext::anonymous())
        .expect_err("exhausted");
    assert_eq!(err.message, "upstream broke");
    assert_eq!(replay.unused(), 0);
    assert_eq!(
        *waits.lock().expect("waits"),
        vec![
            Duration::from_millis(100),
            Duration::from_millis(500),
            Duration::from_millis(1500)
        ]
    );
}

#[test]
fn an_absent_key_is_reported_as_unconfigured_when_registered() {
    use orbital_integrations::error::NOT_CONFIGURED;
    use orbital_integrations::Integrations;
    let creds = Credentials::default();
    let mut integrations = Integrations::new(&creds).expect("mode");
    let transport: Arc<dyn Transport> = Replay::new(&fixture(youtube::SERVICE, "search"), &[]);
    youtube::register(&mut integrations, &creds, transport);
    let err = integrations
        .call_json(
            youtube::SERVICE,
            "search",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .expect_err("unconfigured");
    assert_eq!(err.code.as_deref(), Some(NOT_CONFIGURED));
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(youtube::SERVICE, &["YOUTUBE_API_KEY"]);
}
