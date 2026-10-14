//! Twin parity: every exchange `@almadar/integrations` recorded for the `drive` provider
//! (`fixtures/drive/*.json`, written by `__tests__/drive-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider, signed JWT assertion included (the clock is the one the recording froze). It must send the
//! same requests (multipart boundaries normalized) and produce the same result or error.

mod c_replay;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c_replay::{
    assert_no_secrets, assert_outcome, fixture_named, fixtures, register, run, Replay, Run,
};
use orbital_integrations::drive::{self, DriveProvider};
use orbital_integrations::{Credentials, Integrations, Provider, Transport};

/// `FROZEN_NOW_MS` in `__tests__/support/google-test-account.ts`.
const FROZEN_NOW_MS: u64 = 1_760_000_000_000;

fn replay_with_waits(fixture: &c_replay::Fixture) -> (Run, Vec<Duration>) {
    let waits = Arc::new(Mutex::new(Vec::new()));
    let sink = waits.clone();
    let outcome = run(
        drive::SERVICE,
        fixture,
        &Replay::default(),
        |integrations: &mut Integrations, creds: &Credentials, transport: Arc<dyn Transport>| {
            register(
                integrations,
                drive::SERVICE,
                drive::is_configured(creds),
                || {
                    DriveProvider::with_runtime(
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
    let all = fixtures("drive");
    assert!(
        all.len() >= 42,
        "expected the recorded drive cases, found {}",
        all.len()
    );
    for fixture in &all {
        let (outcome, _) = replay_with_waits(fixture);
        assert_outcome(fixture, &outcome);
    }
}

#[test]
fn retries_wait_the_gaxios_backoff_including_the_token_exchanges() {
    let one = vec![Duration::from_millis(100)];
    for (case, expected) in [
        ("list-files-retried-then-ok", one.clone()),
        ("jwt-token-retried-on-server-error", one.clone()),
        ("refresh-token-retried-on-server-error", one),
        ("list-files-forbidden", vec![]),
        ("upload-file", vec![]),
    ] {
        let (_, waits) = replay_with_waits(&fixture_named("drive", case));
        assert_eq!(waits, expected, "{case}");
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut query = fixture_named("drive", "list-files-filters");
    query.exchanges[1].request.url = query.exchanges[1]
        .request
        .url
        .replace("pageSize=5", "pageSize=6");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&query)).is_err(),
        "a different query must fail the replay"
    );

    let mut multipart = fixture_named("drive", "upload-file");
    multipart.exchanges[1].request.body = multipart.exchanges[1]
        .request
        .body
        .replace("hello", "howdy");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&multipart)).is_err(),
        "a different upload body must fail the replay"
    );

    let mut refresh = fixture_named("drive", "upload-file");
    refresh.exchanges[0].request.body = refresh.exchanges[0]
        .request
        .body
        .replace("test-client-id", "other-client");
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&refresh)).is_err(),
        "a different refresh request must fail the replay"
    );

    let mut writer = fixture_named("drive", "upload-file-user-token-wins-writes-when-both");
    writer.exchanges[0].request.url = "https://www.googleapis.com/oauth2/v4/token".into();
    assert!(
        std::panic::catch_unwind(|| replay_with_waits(&writer)).is_err(),
        "writes must use the user's token when both clients exist"
    );
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets("drive");
}
