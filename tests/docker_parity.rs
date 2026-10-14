//! Twin parity: every session `@almadar/integrations` recorded for the `docker` provider
//! (`fixtures/docker/*.json`, written by `__tests__/docker-recorded-exchanges.test.ts`) is replayed against the
//! Rust provider under the clock and random draws it was recorded with.

mod infra_support;

use orbital_integrations::CallContext;
use std::sync::Arc;

use infra_support::{
    assert_no_secrets, fixture, fixtures, replay, Fixture, ScriptedClock, ScriptedRand,
};
use orbital_integrations::docker::{self, DockerProvider};
use orbital_integrations::{Credentials, Integrations};

fn play(fixture: &Fixture) {
    let creds = Credentials::from_env(fixture.env.clone());
    let clock = ScriptedClock::new();
    let rand = Arc::new(ScriptedRand::new());
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(
        docker::SERVICE,
        Arc::new(DockerProvider::with_sources(clock.clock(), rand.clone())),
    );
    replay(fixture, docker::SERVICE, &integrations, &clock, &rand, None);
}

#[test]
fn every_recorded_session_replays_to_the_same_results() {
    let all = fixtures("docker");
    assert!(
        all.len() >= 8,
        "expected the recorded docker sessions, found {}",
        all.len()
    );
    for fixture in &all {
        play(fixture);
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture("docker", "logs");
    drifted.steps[0].clock_ms += 1_000;
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(
        outcome.is_err(),
        "a different clock must change the recorded result"
    );
}

#[test]
fn control_a_container_id_comes_from_the_recorded_random_draw() {
    let mut drifted = fixture("docker", "logs");
    drifted.steps[0].randoms[0] = 0.999;
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(
        outcome.is_err(),
        "a different draw must change the container id"
    );
}

#[test]
fn docker_needs_no_credential_so_it_is_always_registered() {
    let none = Credentials::default();
    let mut integrations = Integrations::new(&none).expect("mode");
    docker::register(&mut integrations, &none);
    let mut params = serde_json::Map::new();
    params.insert("all".into(), true.into());
    let listed = integrations
        .call_json("docker", "list", &params, &CallContext::anonymous())
        .expect("list");
    assert_eq!(listed, serde_json::json!({ "containers": [], "total": 0 }));
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(&fixtures("docker"));
}
