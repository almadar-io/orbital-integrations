//! Twin parity for the `webhook` provider: every case `__tests__/webhook-recorded-exchanges.test.ts`
//! recorded (`fixtures/webhook/*.json`) replays to the same result or error under the recorded clock.

mod l13d_common;

use std::sync::Arc;
use std::time::Duration;

use l13d_common::*;
use orbital_integrations::hmacsha256::hmac_sha256_hex;
use orbital_integrations::webhook::{self, WebhookProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

const HEADERS: [&str; 2] = ["x-almadar-event", "x-almadar-signature"];

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(
        WebhookProvider::new(creds, transport).with_clock(clock_of(fixture)),
    ))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(webhook::SERVICE, 16, &HEADERS, build);
}

#[test]
fn control_a_signature_over_another_body_is_caught() {
    let mut drifted = fixture(webhook::SERVICE, "send-signed-with-the-env-secret");
    drifted.now = Some("2026-05-17T08:09:11.011Z".to_string());
    let outcome = std::panic::catch_unwind(|| run(&drifted, &HEADERS, build));
    assert!(
        outcome.is_err(),
        "a different timestamp changes body and signature"
    );
}

#[test]
fn the_recorded_signature_is_the_hmac_of_the_recorded_body() {
    for case in [
        ("send-signed-with-the-env-secret", "test-key"),
        ("send-signed-with-a-per-call-secret", "call-secret"),
    ] {
        let f = fixture(webhook::SERVICE, case.0);
        let request = &f.exchanges[0].request;
        let expected = format!("sha256={}", hmac_sha256_hex(case.1, &request.body));
        assert_eq!(
            request.headers["x-almadar-signature"], expected,
            "{}",
            case.0
        );
    }
}

#[test]
fn an_unsigned_call_sends_no_signature_header() {
    let f = fixture(webhook::SERVICE, "send-unsigned-without-any-secret");
    let (_, replay) = run(&f, &HEADERS, build);
    let sent = replay.sent.lock().expect("sent");
    assert!(sent[0]
        .headers
        .iter()
        .all(|(k, _)| k != "X-Almadar-Signature"));
}

#[test]
fn the_timeout_comes_from_the_env_and_an_unreadable_one_falls_back() {
    for (case, expected) in [
        ("send-with-a-configured-timeout", 2500),
        ("send-with-an-unreadable-timeout-uses-the-default", 10_000),
        ("send-signed-with-the-env-secret", 10_000),
    ] {
        let f = fixture(webhook::SERVICE, case);
        let (_, replay) = run(&f, &HEADERS, build);
        assert_eq!(
            replay.sent.lock().expect("sent")[0].timeout,
            Some(Duration::from_millis(expected)),
            "{case}"
        );
    }
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(webhook::SERVICE, &["WEBHOOK_SIGNING_SECRET"]);
}
