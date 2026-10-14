//! Twin parity: every session `@almadar/integrations` recorded for the `queue` provider
//! (`fixtures/queue/*.json`, written by `__tests__/queue-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider under the clock and random draws it was recorded with.

mod infra_support;

use orbital_integrations::CallContext;
use std::sync::Arc;

use infra_support::{
    assert_no_secrets, fixture, fixtures, replay, Fixture, ScriptedClock, ScriptedRand,
};
use orbital_integrations::queue::{self, QueueProvider};
use orbital_integrations::{Credentials, Integrations};

fn play(fixture: &Fixture) {
    let creds = Credentials::from_env(fixture.env.clone());
    let clock = ScriptedClock::new();
    let rand = Arc::new(ScriptedRand::new());
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(
        queue::SERVICE,
        Arc::new(QueueProvider::with_sources(clock.clock(), rand.clone())),
    );
    replay(fixture, queue::SERVICE, &integrations, &clock, &rand, None);
}

#[test]
fn every_recorded_session_replays_to_the_same_results() {
    let all = fixtures("queue");
    assert!(
        all.len() >= 6,
        "expected the recorded queue sessions, found {}",
        all.len()
    );
    for fixture in &all {
        play(fixture);
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture("queue", "delay");
    drifted.steps[3].clock_ms = drifted.steps[4].clock_ms;
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(
        outcome.is_err(),
        "a different clock must change the recorded result"
    );
}

#[test]
fn control_a_job_id_comes_from_the_recorded_random_draw() {
    let mut drifted = fixture("queue", "priority-order");
    drifted.steps[0].randoms[0] = 0.999;
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(outcome.is_err(), "a different draw must change the job id");
}

#[test]
fn queue_is_registered_only_when_queue_url_is_declared() {
    let none = Credentials::default();
    let mut integrations = Integrations::new(&none).expect("mode");
    queue::register(&mut integrations, &none);
    let err = integrations
        .call_json(
            "queue",
            "size",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("NOT_CONFIGURED"));

    let configured = Credentials::from_env(
        [("QUEUE_URL".to_string(), "amqp://x".to_string())]
            .into_iter()
            .collect(),
    );
    let mut integrations = Integrations::new(&configured).expect("mode");
    queue::register(&mut integrations, &configured);
    let mut params = serde_json::Map::new();
    params.insert("queue".into(), "q".into());
    assert!(integrations
        .call_json("queue", "size", &params, &CallContext::anonymous())
        .is_ok());
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(&fixtures("queue"));
}
