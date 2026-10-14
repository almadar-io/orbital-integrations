//! Twin parity for the `ml` provider: every case `__tests__/ml-recorded-exchanges.test.ts`
//! recorded (`fixtures/ml/*.json`) replays to the same result or error.

mod l13d_common;

use orbital_integrations::CallContext;
use std::sync::Arc;
use std::time::Duration;

use l13d_common::*;
use orbital_integrations::ml::{self, MlProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    _fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(MlProvider::new(creds, transport)?))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(ml::SERVICE, 24, &[], build);
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(ml::SERVICE, "infer");
    drifted.exchanges[0].request.body = drifted.exchanges[0]
        .request
        .body
        .replace("distilbert", "bert");
    let outcome = std::panic::catch_unwind(|| run(&drifted, &[], build));
    assert!(outcome.is_err(), "a different model must fail the replay");
}

#[test]
fn the_timeout_comes_from_the_env_and_defaults_to_a_cold_start_budget() {
    for (case, expected) in [("infer-with-a-configured-timeout", 4500), ("infer", 60_000)] {
        let f = fixture(ml::SERVICE, case);
        let (_, replay) = run(&f, &[], build);
        assert_eq!(
            replay.sent.lock().expect("sent")[0].timeout,
            Some(Duration::from_millis(expected)),
            "{case}"
        );
    }
}

#[test]
fn an_unconfigured_service_is_reported_when_registered() {
    use orbital_integrations::error::NOT_CONFIGURED;
    use orbital_integrations::Integrations;
    let creds = Credentials::default();
    let mut integrations = Integrations::new(&creds).expect("mode");
    let transport: Arc<dyn Transport> = Replay::new(&fixture(ml::SERVICE, "infer"), &[]);
    ml::register(&mut integrations, &creds, transport);
    let err = integrations
        .call_json(
            ml::SERVICE,
            "infer",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .expect_err("unconfigured");
    assert_eq!(err.code.as_deref(), Some(NOT_CONFIGURED));
}
