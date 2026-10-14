//! Twin parity for the `arxiv` provider: every case `__tests__/arxiv-recorded-exchanges.test.ts`
//! recorded (`fixtures/arxiv/*.json`) replays to the same result or error.

mod l13d_common;

use std::sync::Arc;

use l13d_common::*;
use orbital_integrations::arxiv::{self, ArxivProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    _fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(ArxivProvider::new(creds, transport)))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(arxiv::SERVICE, 10, &[], build);
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(arxiv::SERVICE, "search");
    drifted.exchanges[0].request.url = drifted.exchanges[0]
        .request
        .url
        .replace("max_results=8", "max_results=9");
    let outcome = std::panic::catch_unwind(|| run(&drifted, &[], build));
    assert!(outcome.is_err(), "a different url must fail the replay");
}

#[test]
fn a_blank_query_is_answered_without_a_request() {
    let blank = fixture(arxiv::SERVICE, "search-blank-query-sends-nothing");
    assert!(blank.exchanges.is_empty());
    let (result, replay) = run(&blank, &[], build);
    check(&blank, result, &replay);
}
