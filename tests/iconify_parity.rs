//! Twin parity for the `iconify` provider: every case `__tests__/iconify-recorded-exchanges.test.ts`
//! recorded (`fixtures/iconify/*.json`) replays to the same result or error.

mod l13d_common;

use std::sync::Arc;
use std::time::Duration;

use l13d_common::*;
use orbital_integrations::iconify::{self, IconifyProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

const HEADERS: [&str; 1] = ["user-agent"];

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    _fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(IconifyProvider::new(creds, transport)))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(iconify::SERVICE, 22, &HEADERS, build);
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(iconify::SERVICE, "search");
    drifted.exchanges[0].request.url = drifted.exchanges[0]
        .request
        .url
        .replace("limit=3", "limit=4");
    let outcome = std::panic::catch_unwind(|| run(&drifted, &HEADERS, build));
    assert!(outcome.is_err(), "a different url must fail the replay");
}

#[test]
fn the_configured_user_agent_and_timeout_reach_the_transport() {
    let custom = fixture(iconify::SERVICE, "svg-exists-custom-user-agent");
    let (_, replay) = run(&custom, &HEADERS, build);
    let sent = replay.sent.lock().expect("sent");
    assert_eq!(sent[0].timeout, Some(Duration::from_millis(2500)));
    assert!(sent[0]
        .headers
        .iter()
        .any(|(k, v)| k == "User-Agent" && v == "Probe/3.1"));
}
