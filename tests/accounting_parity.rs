//! Twin parity for the `accounting` provider: every case `__tests__/accounting-recorded-exchanges.test.ts`
//! recorded (`fixtures/accounting/*.json`) replays to the same result or error under the recorded clock.

mod l13d_common;

use std::sync::Arc;

use l13d_common::*;
use orbital_integrations::accounting::{self, AccountingProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

fn build(
    _creds: &Credentials,
    _transport: Arc<dyn Transport>,
    fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(
        AccountingProvider::new().with_clock(clock_of(fixture)),
    ))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(accounting::SERVICE, 12, &[], build);
}

#[test]
fn control_a_filename_stamped_by_another_day_is_caught() {
    let mut drifted = fixture(accounting::SERVICE, "export-invoices");
    drifted.now = Some("2026-03-10T00:00:00.000Z".to_string());
    let outcome = std::panic::catch_unwind(|| {
        let (result, replay) = run(&drifted, &[], build);
        check(&drifted, result, &replay);
    });
    assert!(outcome.is_err(), "a different day must change the filename");
}
