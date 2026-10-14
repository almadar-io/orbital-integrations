//! Twin parity: every exchange `@almadar/integrations` recorded for the `esign` provider
//! (`fixtures/esign/*.json`, written by `__tests__/esign-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider. It must send the same requests and produce the same result or error.

mod c_replay;

use std::sync::Arc;

use c_replay::{
    assert_no_secrets, assert_outcome, fixture_named, fixtures, register, run, Replay, Run,
};
use orbital_integrations::esign::{self, EsignProvider};
use orbital_integrations::{Credentials, Integrations, Provider, Transport};

fn replay(fixture: &c_replay::Fixture) -> Run {
    run(
        esign::SERVICE,
        fixture,
        &Replay::default(),
        |integrations: &mut Integrations, creds: &Credentials, transport: Arc<dyn Transport>| {
            register(
                integrations,
                esign::SERVICE,
                esign::is_configured(creds),
                || EsignProvider::new(creds, transport).map(|p| Arc::new(p) as Arc<dyn Provider>),
            );
        },
    )
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures("esign");
    assert!(
        all.len() >= 18,
        "expected the recorded esign cases, found {}",
        all.len()
    );
    for fixture in &all {
        assert_outcome(fixture, &replay(fixture));
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture_named("esign", "send-envelope");
    drifted.exchanges[0].request.body = drifted.exchanges[0]
        .request
        .body
        .replace("\"routingOrder\":\"1\"", "\"routingOrder\":\"2\"");
    assert!(
        std::panic::catch_unwind(|| replay(&drifted)).is_err(),
        "a different request body must fail the replay"
    );

    let mut moved = fixture_named("esign", "get-envelope-status-completed");
    moved.exchanges[0].request.url = moved.exchanges[0]
        .request
        .url
        .replace("/envelopes/", "/envelope/");
    assert!(
        std::panic::catch_unwind(|| replay(&moved)).is_err(),
        "a different url must fail the replay"
    );
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets("esign");
}
