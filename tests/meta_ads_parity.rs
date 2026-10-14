//! Twin parity for the `metaAds` provider: every case `__tests__/metaads-recorded-exchanges.test.ts`
//! recorded (`fixtures/metaAds/*.json`) replays to the same result or error.

mod l13d_common;

use orbital_integrations::CallContext;
use std::sync::Arc;

use l13d_common::*;
use orbital_integrations::meta_ads::{self, MetaAdsProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    _fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(MetaAdsProvider::new(creds, transport)?))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(meta_ads::SERVICE, 21, &[], build);
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(meta_ads::SERVICE, "list-campaigns-status-filter");
    drifted.exchanges[0].request.url = drifted.exchanges[0]
        .request
        .url
        .replace("%5B%22ACTIVE%22%5D", "%5B%22PAUSED%22%5D");
    let outcome = std::panic::catch_unwind(|| run(&drifted, &[], build));
    assert!(
        outcome.is_err(),
        "a different status filter must fail the replay"
    );
}

#[test]
fn an_absent_token_is_reported_as_unconfigured_when_registered() {
    use orbital_integrations::error::NOT_CONFIGURED;
    use orbital_integrations::Integrations;
    let creds = Credentials::default();
    let mut integrations = Integrations::new(&creds).expect("mode");
    let transport: Arc<dyn Transport> = Replay::new(&fixture(meta_ads::SERVICE, "get-spend"), &[]);
    meta_ads::register(&mut integrations, &creds, transport);
    let err = integrations
        .call_json(
            meta_ads::SERVICE,
            "getSpend",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .expect_err("unconfigured");
    assert_eq!(err.code.as_deref(), Some(NOT_CONFIGURED));
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(meta_ads::SERVICE, &["META_ACCESS_TOKEN"]);
}
