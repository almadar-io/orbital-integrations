//! Twin parity: every exchange `@almadar/integrations` recorded for the `banking` provider
//! (`fixtures/banking/*.json`, written by `__tests__/banking-recorded-exchanges.test.ts`) is replayed against
//! the Rust provider. It must send the same requests and produce the same result or error.

mod c_replay;

use orbital_integrations::CallContext;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use c_replay::{
    assert_no_secrets, assert_outcome, fixture_named, fixtures, register, run, transport_for,
    Replay, Run,
};
use orbital_integrations::banking::{self, BankingProvider};
use orbital_integrations::jswire::Clock;
use orbital_integrations::{Credentials, Integrations, Provider, Transport};

fn replay(fixture: &c_replay::Fixture) -> Run {
    run(
        banking::SERVICE,
        fixture,
        &Replay::default(),
        |integrations: &mut Integrations, creds: &Credentials, transport: Arc<dyn Transport>| {
            register(
                integrations,
                banking::SERVICE,
                banking::is_configured(creds),
                || BankingProvider::new(creds, transport).map(|p| Arc::new(p) as Arc<dyn Provider>),
            );
        },
    )
}

#[test]
fn every_recorded_exchange_replays_to_the_same_result() {
    let all = fixtures("banking");
    assert!(
        all.len() >= 20,
        "expected the recorded banking cases, found {}",
        all.len()
    );
    for fixture in &all {
        assert_outcome(fixture, &replay(fixture));
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture_named("banking", "create-requisition");
    drifted.exchanges[1].request.body = drifted.exchanges[1]
        .request
        .body
        .replace("institution_id", "institutionId");
    assert!(
        std::panic::catch_unwind(|| replay(&drifted)).is_err(),
        "a different request body must fail the replay"
    );

    let mut moved = fixture_named("banking", "list-transactions-date-range");
    moved.exchanges[1].request.url = moved.exchanges[1].request.url.replace("date_to", "dateTo");
    assert!(
        std::panic::catch_unwind(|| replay(&moved)).is_err(),
        "a different query must fail the replay"
    );
}

fn two_calls(clock: Clock) -> usize {
    let fixture = fixture_named("banking", "list-accounts");
    let mut doubled = fixture.clone();
    doubled.exchanges.push(fixture.exchanges[1].clone());
    let replay = Replay::default();
    let transport = transport_for(&doubled, &replay);
    let creds = c_replay::credentials(&doubled, &replay);
    let provider = BankingProvider::new(&creds, transport.clone() as Arc<dyn Transport>)
        .expect("provider")
        .with_clock(clock);
    for _ in 0..2 {
        provider
            .call("listAccounts", &doubled.params, &CallContext::anonymous())
            .expect("call");
    }
    let left = transport.script.lock().expect("script").len();
    left
}

#[test]
fn the_access_token_is_minted_once_and_reused() {
    let unused = two_calls(Arc::new(|| 1_000));
    assert_eq!(unused, 0);
}

#[test]
fn control_an_expired_token_is_minted_again() {
    let now = Arc::new(AtomicU64::new(1_000));
    let ticking = now.clone();
    let clock: Clock = Arc::new(move || ticking.fetch_add(86_400_000, Ordering::SeqCst));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| two_calls(clock)));
    assert!(
        result.is_err(),
        "a second token request is not in the recording"
    );
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets("banking");
}
