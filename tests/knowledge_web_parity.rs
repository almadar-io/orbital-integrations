//! Twin parity for the browser leg of `knowledge` (`createKnowledgeService`): every case
//! `__tests__/knowledge-web-recorded-exchanges.test.ts` recorded (`fixtures/knowledge-web/*.json`) replays to
//! the same requests and the same result or error. The service is driven directly, as the TS test does, so the
//! contract layer's own messages do not stand in for the service's.

mod l13d_common;

use orbital_integrations::CallContext;
use std::sync::Arc;

use l13d_common::*;
use orbital_integrations::knowledge::web::{Connection, KnowledgeAuth, WebProvider, AUTH_ERROR};
use orbital_integrations::wallclock::Clock;
use orbital_integrations::{
    HttpRequest, HttpResponse, IntegrationError, Provider, Transport, TransportError,
};

const SERVICE: &str = "knowledge-web";

struct StaticAuth {
    connected: bool,
    account: String,
}

impl KnowledgeAuth for StaticAuth {
    fn status(&self) -> Result<Connection, IntegrationError> {
        Ok(Connection {
            connected: self.connected,
            account: self.account.clone(),
        })
    }
    fn connect(&self) -> Result<Connection, IntegrationError> {
        Ok(Connection {
            connected: true,
            account: "me@example.com".to_string(),
        })
    }
    fn id_token(&self) -> Result<String, IntegrationError> {
        if self.connected {
            Ok("test-key".to_string())
        } else {
            Err(IntegrationError::coded(AUTH_ERROR, "Not connected"))
        }
    }
}

fn provider(fixture: &Fixture, transport: Arc<dyn Transport>) -> WebProvider {
    let auth = StaticAuth {
        connected: fixture
            .env
            .get("KNOWLEDGE_TEST_CONNECTED")
            .is_some_and(|v| v == "true"),
        account: fixture
            .env
            .get("KNOWLEDGE_TEST_ACCOUNT")
            .cloned()
            .unwrap_or_default(),
    };
    let clock: Clock = clock_of(fixture);
    WebProvider::new(MOCK_BASE, Arc::new(auth), transport).with_clock(clock)
}

fn replay(fixture: &Fixture) {
    let transport = Replay::new(fixture, &[]);
    let shared: Arc<dyn Transport> = transport.clone();
    let result =
        provider(fixture, shared).call(&fixture.action, &fixture.params, &CallContext::anonymous());
    check(fixture, result, &transport);
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    let all = fixtures(SERVICE);
    assert!(
        all.len() >= 16,
        "expected the recorded knowledge-web cases, found {}",
        all.len()
    );
    for fixture in &all {
        replay(fixture);
    }
}

#[test]
fn control_a_request_that_drifts_from_the_recording_is_caught() {
    let mut drifted = fixture(SERVICE, "topics-compose");
    drifted.exchanges[2].request.url = drifted.exchanges[2]
        .request
        .url
        .replace("limit=50", "limit=51");
    assert!(std::panic::catch_unwind(|| replay(&drifted)).is_err());
}

#[test]
fn control_a_drifted_result_is_caught() {
    let mut drifted = fixture(SERVICE, "topics-limit-one");
    if let Outcome::Ok { data } = &mut drifted.outcome {
        data["topics"][0]["label"] = "Somebody else".into();
    }
    assert!(std::panic::catch_unwind(|| replay(&drifted)).is_err());
}

#[test]
fn no_secret_reaches_a_fixture() {
    assert_no_secrets(SERVICE, &[]);
    for fixture in fixtures(SERVICE) {
        for exchange in &fixture.exchanges {
            assert_eq!(
                exchange
                    .request
                    .headers
                    .get("authorization")
                    .map(String::as_str),
                Some(REDACTED),
                "{}",
                fixture.case
            );
        }
    }
}

struct Dead;

impl Transport for Dead {
    fn send(&self, _request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        Err(TransportError::Failed("refused".to_string()))
    }
}

#[test]
fn an_unreachable_server_is_a_network_error_naming_the_route() {
    let live = fixture(SERVICE, "topics-compose");
    let err = provider(&live, Arc::new(Dead))
        .call("currentTopics", &live.params, &CallContext::anonymous())
        .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("NETWORK_ERROR"));
    assert_eq!(err.message, "KFlow /api/user/jump-back-in is unreachable");
}

#[test]
fn control_a_malformed_base_url_is_unreachable_too() {
    let live = fixture(SERVICE, "topics-compose");
    let auth = StaticAuth {
        connected: true,
        account: String::new(),
    };
    let provider = WebProvider::new("not a url", Arc::new(auth), Arc::new(Dead));
    let err = provider
        .call("currentTopics", &live.params, &CallContext::anonymous())
        .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("NETWORK_ERROR"));
}
