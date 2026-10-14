//! Twin parity for the `analytics` provider: every case `__tests__/analytics-recorded-exchanges.test.ts`
//! recorded (`fixtures/analytics/*.json`) replays to the same result or error, from the page it recorded.

mod l13d_common;

use std::sync::Arc;

use l13d_common::*;
use orbital_integrations::analytics::{self, AnalyticsProvider, PagePort};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};

struct RecordedPage(Page);

impl PagePort for RecordedPage {
    fn hostname(&self) -> String {
        self.0.hostname.clone()
    }
    fn referrer(&self) -> String {
        self.0.referrer.clone()
    }
    fn screen_width(&self) -> Option<f64> {
        self.0.screen_width
    }
}

fn build(
    _creds: &Credentials,
    transport: Arc<dyn Transport>,
    fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    let page = fixture
        .page
        .clone()
        .expect("every analytics fixture records its page");
    Ok(Arc::new(
        AnalyticsProvider::new(transport).with_page(Arc::new(RecordedPage(page))),
    ))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(analytics::SERVICE, 23, &[], build);
}

#[test]
fn control_a_payload_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(analytics::SERVICE, "pageview");
    drifted.page.as_mut().expect("page").referrer = "https://other.example/".to_string();
    let outcome = std::panic::catch_unwind(|| run(&drifted, &[], build));
    assert!(
        outcome.is_err(),
        "a different referrer must fail the replay"
    );
}

#[test]
fn control_the_same_visit_from_a_local_host_posts_nothing() {
    let mut local = fixture(analytics::SERVICE, "pageview");
    local.page.as_mut().expect("page").hostname = "localhost".to_string();
    local.exchanges.clear();
    local.outcome = Outcome::Ok {
        data: serde_json::json!({ "sent": false }),
    };
    let (result, replay) = run(&local, &[], build);
    check(&local, result, &replay);
}
