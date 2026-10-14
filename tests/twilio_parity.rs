//! Twin parity for the `twilio` provider: every case `__tests__/twilio-recorded-exchanges.test.ts`
//! recorded (`fixtures/twilio/*.json`) replays to the same result or error.

mod l13d_common;

use orbital_integrations::CallContext;
use std::sync::Arc;
use std::time::Duration;

use l13d_common::*;
use orbital_integrations::twilio::{self, TwilioProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    _fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(TwilioProvider::new(creds, transport)?))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(twilio::SERVICE, 24, &[], build);
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(twilio::SERVICE, "send-sms");
    drifted.exchanges[0].request.body = drifted.exchanges[0]
        .request
        .body
        .replace("481516", "481517");
    let outcome = std::panic::catch_unwind(|| run(&drifted, &[], build));
    assert!(outcome.is_err(), "a different body must fail the replay");
}

#[test]
fn the_request_carries_basic_auth_and_the_sdk_timeout() {
    let f = fixture(twilio::SERVICE, "send-sms");
    let (_, replay) = run(&f, &[], build);
    let sent = replay.sent.lock().expect("sent");
    assert_eq!(sent[0].timeout, Some(Duration::from_millis(30_000)));
    let auth = &sent[0]
        .headers
        .iter()
        .find(|(k, _)| k == "Authorization")
        .expect("auth")
        .1;
    assert_eq!(
        auth,
        "Basic QUMwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDp0ZXN0LWtleQ=="
    );
}

#[test]
fn an_unconfigured_service_is_reported_when_registered() {
    use orbital_integrations::error::NOT_CONFIGURED;
    use orbital_integrations::Integrations;
    let creds = Credentials::default();
    let mut integrations = Integrations::new(&creds).expect("mode");
    let transport: Arc<dyn Transport> = Replay::new(&fixture(twilio::SERVICE, "send-sms"), &[]);
    twilio::register(&mut integrations, &creds, transport);
    let err = integrations
        .call_json(
            twilio::SERVICE,
            "sendSMS",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .expect_err("unconfigured");
    assert_eq!(err.code.as_deref(), Some(NOT_CONFIGURED));
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(twilio::SERVICE, &["TWILIO_AUTH_TOKEN"]);
}
