//! Twin parity: every exchange `@almadar/integrations` recorded for the `email` provider
//! (`fixtures/email/*.json`, written by `__tests__/email-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider. It must send the same requests and produce the same result or error.

mod c_replay;

use std::sync::Arc;

use c_replay::{
    assert_no_secrets, assert_outcome, fixture_named, fixtures, register, run, Replay, Run,
};
use orbital_integrations::email::{self, EmailProvider};
use orbital_integrations::{Credentials, Integrations, Provider, Transport};

fn replay(fixture: &c_replay::Fixture) -> Run {
    run(
        email::SERVICE,
        fixture,
        &Replay::default(),
        |integrations: &mut Integrations, creds: &Credentials, transport: Arc<dyn Transport>| {
            register(
                integrations,
                email::SERVICE,
                email::is_configured(creds),
                || EmailProvider::new(creds, transport).map(|p| Arc::new(p) as Arc<dyn Provider>),
            );
        },
    )
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures("email");
    assert!(
        all.len() >= 25,
        "expected the recorded email cases, found {}",
        all.len()
    );
    for fixture in &all {
        assert_outcome(fixture, &replay(fixture));
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture_named("email", "sendgrid-send");
    drifted.exchanges[0].request.body = drifted.exchanges[0]
        .request
        .body
        .replace("text/html", "text/plain");
    assert!(
        std::panic::catch_unwind(|| replay(&drifted)).is_err(),
        "a different request body must fail the replay"
    );

    let mut resend = fixture_named("email", "resend-send");
    resend.exchanges[0].request.url = resend.exchanges[0].request.url.replace("/emails", "/email");
    assert!(
        std::panic::catch_unwind(|| replay(&resend)).is_err(),
        "a different url must fail the replay"
    );
}

#[test]
fn sendgrid_wins_when_both_keys_are_set_and_control_resend_serves_alone() {
    let both = fixture_named("email", "sendgrid-wins-when-both-keys-are-set");
    assert_eq!(
        email::select_vendor(&c_replay::credentials(&both, &Replay::default())),
        Some(email::Vendor::SendGrid)
    );
    let alone = fixture_named("email", "resend-send");
    assert_eq!(
        email::select_vendor(&c_replay::credentials(&alone, &Replay::default())),
        Some(email::Vendor::Resend)
    );
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets("email");
}
