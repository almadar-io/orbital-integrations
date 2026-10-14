//! Twin parity for the `wikimedia` provider: every case `__tests__/wikimedia-recorded-exchanges.test.ts`
//! recorded (`fixtures/wikimedia/*.json`) replays to the same result or error.

mod l13d_common;

use std::sync::Arc;
use std::time::Duration;

use l13d_common::*;
use orbital_integrations::wikimedia::{self, WikimediaProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

const HEADERS: [&str; 1] = ["user-agent"];

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    _fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(WikimediaProvider::new(creds, transport)))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(wikimedia::SERVICE, 16, &HEADERS, build);
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(wikimedia::SERVICE, "get-page");
    drifted.exchanges[0]
        .request
        .headers
        .insert("user-agent".to_string(), "Other/1.0".to_string());
    let outcome = std::panic::catch_unwind(|| run(&drifted, &HEADERS, build));
    assert!(
        outcome.is_err(),
        "a different user agent must fail the replay"
    );
}

#[test]
fn the_configured_timeout_reaches_the_transport() {
    let custom = fixture(wikimedia::SERVICE, "get-page-custom-user-agent");
    let (_, replay) = run(&custom, &HEADERS, build);
    let sent = replay.sent.lock().expect("sent");
    assert_eq!(sent[0].timeout, Some(Duration::from_millis(1500)));
    let default = fixture(wikimedia::SERVICE, "get-page");
    let (_, replay) = run(&default, &HEADERS, build);
    assert_eq!(
        replay.sent.lock().expect("sent")[0].timeout,
        Some(Duration::from_millis(6000))
    );
}
