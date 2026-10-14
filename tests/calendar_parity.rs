//! Twin parity: every exchange `@almadar/integrations` recorded for the `calendar` provider
//! (`fixtures/calendar/*.json`, written by `__tests__/calendar-recorded-exchanges.test.ts`) is replayed against
//! the Rust provider, signed JWT assertion included (the clock is the one the recording froze). It must send the
//! same requests and produce the same result or error.

mod c_replay;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c_replay::{
    assert_no_secrets, assert_outcome, fixture_named, fixtures, register, run, Replay, Run,
};
use orbital_integrations::calendar::{self, CalendarProvider};
use orbital_integrations::{Credentials, Integrations, Provider, Transport};

/// `FROZEN_NOW_MS` in `__tests__/support/google-test-account.ts`.
const FROZEN_NOW_MS: u64 = 1_760_000_000_000;

fn replay_with_waits(fixture: &c_replay::Fixture) -> (Run, Vec<Duration>) {
    let waits = Arc::new(Mutex::new(Vec::new()));
    let sink = waits.clone();
    let outcome = run(
        calendar::SERVICE,
        fixture,
        &Replay::default(),
        |integrations: &mut Integrations, creds: &Credentials, transport: Arc<dyn Transport>| {
            register(
                integrations,
                calendar::SERVICE,
                calendar::is_configured(creds),
                || {
                    CalendarProvider::with_runtime(
                        creds,
                        transport,
                        Arc::new(|| FROZEN_NOW_MS),
                        Arc::new(move |d| sink.lock().expect("waits").push(d)),
                    )
                    .map(|p| Arc::new(p) as Arc<dyn Provider>)
                },
            );
        },
    );
    let recorded = waits.lock().expect("waits").clone();
    (outcome, recorded)
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures("calendar");
    assert!(
        all.len() >= 44,
        "expected the recorded calendar cases, found {}",
        all.len()
    );
    for fixture in &all {
        let (outcome, _) = replay_with_waits(fixture);
        assert_outcome(fixture, &outcome);
    }
}

#[test]
fn retries_wait_the_gaxios_backoff_for_retryable_methods_only() {
    let ms = |list: &[u64]| {
        list.iter()
            .map(|m| Duration::from_millis(*m))
            .collect::<Vec<_>>()
    };
    for (case, expected) in [
        ("list-events-rate-limited-then-ok", ms(&[100])),
        ("list-events-retries-exhausted", ms(&[100, 500, 1500])),
        ("delete-event-retried-on-server-error", ms(&[100])),
        ("create-event-server-error-is-not-retried", ms(&[])),
        ("list-events-unauthorized", ms(&[])),
        ("list-events-window", ms(&[])),
    ] {
        let (_, waits) = replay_with_waits(&fixture_named("calendar", case));
        assert_eq!(waits, expected, "{case}");
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut query = fixture_named("calendar", "list-events-window");
    query.exchanges[1].request.url = query.exchanges[1]
        .request
        .url
        .replace("singleEvents=true", "singleEvents=false");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&query)).is_err(),
        "a different query must fail the replay"
    );

    let mut assertion = fixture_named("calendar", "list-events-window");
    assertion.exchanges[0].request.body =
        assertion.exchanges[0]
            .request
            .body
            .replacen("assertion=eyJ", "assertion=eyK", 1);
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&assertion)).is_err(),
        "a different JWT assertion must fail the replay"
    );

    let mut delegated = fixture_named("calendar", "list-events-delegated-subject");
    delegated.env.remove("GOOGLE_CALENDAR_SUBJECT");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&delegated)).is_err(),
        "a missing delegation subject changes the assertion"
    );

    let mut body = fixture_named("calendar", "create-event-with-end");
    body.exchanges[1].request.body = body.exchanges[1].request.body.replace("Zoom", "Teams");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&body)).is_err(),
        "a different request body must fail the replay"
    );
}

#[test]
fn a_raw_json_service_account_key_is_accepted_like_the_base64_one() {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let mut fixture = fixture_named("calendar", "list-events-defaults");
    let encoded = fixture
        .env
        .get("GOOGLE_CALENDAR_SA_KEY")
        .expect("key")
        .clone();
    let raw = String::from_utf8(STANDARD.decode(encoded).expect("base64")).expect("utf8");
    fixture.env.insert("GOOGLE_CALENDAR_SA_KEY".into(), raw);
    let (outcome, _) = replay_with_waits(&fixture);
    assert_outcome(&fixture, &outcome);
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets("calendar");
}
