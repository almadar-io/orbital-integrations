//! Twin parity: every session `@almadar/integrations` recorded for the `otel` provider
//! (`fixtures/otel/*.json`, written by `__tests__/otel-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider under the clock and random draws it was recorded with.

mod infra_support;

use orbital_integrations::CallContext;
use std::sync::Arc;

use infra_support::{
    assert_no_secrets, fixture, fixtures, replay, Fixture, ScriptedClock, ScriptedRand,
};
use orbital_integrations::otel::{self, OtelProvider};
use orbital_integrations::{Credentials, Integrations};

fn play(fixture: &Fixture) {
    let creds = Credentials::from_env(fixture.env.clone());
    let clock = ScriptedClock::new();
    let rand = Arc::new(ScriptedRand::new());
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(
        otel::SERVICE,
        Arc::new(OtelProvider::with_sources(clock.clock(), rand.clone())),
    );
    replay(fixture, otel::SERVICE, &integrations, &clock, &rand, None);
}

#[test]
fn every_recorded_session_replays_to_the_same_results() {
    let all = fixtures("otel");
    assert!(
        all.len() >= 5,
        "expected the recorded otel sessions, found {}",
        all.len()
    );
    for fixture in &all {
        play(fixture);
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture("otel", "span-lifecycle");
    drifted.steps[2].clock_ms += 1;
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(
        outcome.is_err(),
        "a different clock must change the recorded result"
    );
}

#[test]
fn control_a_span_id_comes_from_the_recorded_random_draw() {
    let mut drifted = fixture("otel", "span-lifecycle");
    drifted.steps[0].randoms[0] = 0.999;
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(outcome.is_err(), "a different draw must change the span id");
}

#[test]
fn otel_is_registered_only_when_the_collector_endpoint_is_declared() {
    let none = Credentials::default();
    let mut integrations = Integrations::new(&none).expect("mode");
    otel::register(&mut integrations, &none);
    let err = integrations
        .call_json(
            "otel",
            "getMetrics",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("NOT_CONFIGURED"));

    let configured = Credentials::from_env(
        [(
            "OTEL_EXPORTER_OTLP_ENDPOINT".to_string(),
            "http://collector:4318".to_string(),
        )]
        .into_iter()
        .collect(),
    );
    let mut integrations = Integrations::new(&configured).expect("mode");
    otel::register(&mut integrations, &configured);
    assert!(integrations
        .call_json(
            "otel",
            "getMetrics",
            &serde_json::Map::new(),
            &CallContext::anonymous()
        )
        .is_ok());
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(&fixtures("otel"));
}
