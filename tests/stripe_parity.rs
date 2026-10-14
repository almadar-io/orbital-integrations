//! Twin parity: every exchange `@almadar/integrations` recorded for the `stripe` provider
//! (`fixtures/stripe/*.json`, written by `__tests__/stripe-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider. It must send the same requests and produce the same result or error.

mod c_replay;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c_replay::{
    assert_no_secrets, assert_outcome, fixture_named, fixtures, register, run, Replay, Run,
};
use orbital_integrations::stripe::{self, StripeProvider};
use orbital_integrations::{Credentials, Integrations, Provider, Transport};

fn replay_config() -> Replay {
    Replay {
        extra_headers: vec!["stripe-version", "idempotency-key"],
        ..Replay::default()
    }
}

fn replay_with_waits(fixture: &c_replay::Fixture) -> (Run, Vec<Duration>) {
    let waits = Arc::new(Mutex::new(Vec::new()));
    let sink = waits.clone();
    let outcome = run(
        stripe::SERVICE,
        fixture,
        &replay_config(),
        |integrations: &mut Integrations, creds: &Credentials, transport: Arc<dyn Transport>| {
            register(
                integrations,
                stripe::SERVICE,
                stripe::is_configured(creds),
                || {
                    StripeProvider::new(creds, transport).map(|p| {
                        Arc::new(
                            p.with_sleeper(Arc::new(move |d| sink.lock().expect("waits").push(d))),
                        ) as Arc<dyn Provider>
                    })
                },
            );
        },
    );
    let recorded = waits.lock().expect("waits").clone();
    (outcome, recorded)
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures("stripe");
    assert!(
        all.len() >= 50,
        "expected the recorded stripe cases, found {}",
        all.len()
    );
    for fixture in &all {
        let (outcome, waits) = replay_with_waits(fixture);
        assert_outcome(fixture, &outcome);
        assert!(
            waits.iter().all(|w| *w == Duration::from_millis(500)),
            "{}: {waits:?}",
            fixture.case
        );
    }
}

#[test]
fn retries_wait_the_recorded_backoff_and_stop_where_the_api_says_so() {
    for (case, waits) in [
        ("server-error-is-retried-then-succeeds", 1),
        ("server-error-retries-exhausted", 2),
        ("conflict-is-retried", 1),
        ("retry-after-header-does-not-lengthen-the-wait", 1),
        ("should-retry-false-stops-the-retry", 0),
        ("rate-limited-is-not-retried", 0),
        ("get-subscription", 0),
    ] {
        let (_, recorded) = replay_with_waits(&fixture_named("stripe", case));
        assert_eq!(recorded.len(), waits, "{case}");
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture_named("stripe", "create-payment-intent-with-metadata");
    drifted.exchanges[0].request.body = drifted.exchanges[0]
        .request
        .body
        .replace("amount=1999", "amount=1998");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&drifted)).is_err(),
        "a different form body must fail the replay"
    );

    let mut versioned = fixture_named("stripe", "get-subscription");
    versioned.exchanges[0]
        .request
        .headers
        .insert("stripe-version".into(), "2020-01-01".into());
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&versioned)).is_err(),
        "a different API version must fail the replay"
    );

    let mut keyless = fixture_named("stripe", "create-customer");
    keyless.exchanges[0]
        .request
        .headers
        .remove("idempotency-key");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&keyless)).is_err(),
        "an unexpected idempotency key must fail the replay"
    );
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets("stripe");
}
