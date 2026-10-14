//! Twin parity: every session `@almadar/integrations` recorded for the `credentials` provider
//! (`fixtures/credentials/*.json`, written by `__tests__/credentials-recorded-exchanges.test.ts`) is replayed
//! against the Rust provider wired to the host state the session ran against: a credential store, a mode
//! switch and a factory of scripted probes, rebuilt from the fixture's `setup`.

mod infra_support;

use orbital_core::effects::ServiceCaller;
use orbital_integrations::CallContext;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use infra_support::{assert_no_secrets, check_outcome, fixture, fixtures, Fixture, Outcome};
use orbital_integrations::credential_admin::{
    self, Catalog, CredentialsProvider, Factory, Hosts, MemoryCredentialStore, ModeControl,
    ProbeReply,
};
use orbital_integrations::mock::Mode;
use orbital_integrations::{Credentials, Integrations};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Setup {
    services: Vec<String>,
    #[serde(rename = "declaredOrder")]
    declared_order: Vec<String>,
    store: Option<StoreSetup>,
    #[serde(rename = "processEnv")]
    process_env: HashMap<String, String>,
    #[serde(rename = "modeControl")]
    mode_control: Option<ModeSetup>,
    factory: Option<FactorySetup>,
}

#[derive(Deserialize)]
struct StoreSetup {
    enabled: bool,
    entries: Vec<EntrySetup>,
}

#[derive(Deserialize)]
struct EntrySetup {
    service: String,
    #[serde(rename = "envVar")]
    env_var: String,
    value: String,
}

#[derive(Deserialize)]
struct ModeSetup {
    mode: String,
    #[serde(rename = "refuseMock")]
    refuse_mock: bool,
}

#[derive(Deserialize)]
struct FactorySetup {
    configured: Vec<String>,
    probes: HashMap<String, ProbeSetup>,
}

#[derive(Deserialize)]
struct ProbeSetup {
    success: bool,
    data: Option<Value>,
    error: Option<String>,
}

struct FakeMode {
    mode: Mutex<Mode>,
    refuse_mock: bool,
}

impl ModeControl for FakeMode {
    fn mode(&self) -> Mode {
        *self.mode.lock().expect("mode")
    }
    fn set_mode(&self, mode: Mode) -> Result<(), String> {
        if mode == Mode::Mock && self.refuse_mock {
            return Err("Mocked integrations are not permitted when NODE_ENV=production.".into());
        }
        *self.mode.lock().expect("mode") = mode;
        Ok(())
    }
}

struct FakeFactory {
    configured: Vec<String>,
    probes: HashMap<String, ProbeSetup>,
}

impl Factory for FakeFactory {
    fn is_configured(&self, service: &str) -> bool {
        self.configured.iter().any(|c| c == service)
    }
    fn execute(
        &self,
        service: &str,
        _action: &str,
        _params: &Value,
        _ctx: &CallContext,
    ) -> ProbeReply {
        let probe = self.probes.get(service).expect("a probe for the service");
        ProbeReply {
            success: probe.success,
            data: probe.data.clone(),
            error: probe.error.clone(),
        }
    }
}

fn play(fixture: &Fixture) {
    let setup: Setup = serde_json::from_value(fixture.setup.clone()).expect("setup");
    let creds = Credentials::from_env(fixture.env.clone());
    let store = setup.store.as_ref().map(|s| {
        let store = MemoryCredentialStore::new(s.enabled);
        if s.enabled {
            for e in &s.entries {
                store.seed(&e.service, &e.env_var, &e.value);
            }
        }
        Arc::new(store)
    });
    let hosts = Hosts {
        store: store
            .map(|s| s as Arc<dyn orbital_integrations::credential_admin::CredentialStorePort>),
        mode: setup.mode_control.as_ref().map(|m| {
            Arc::new(FakeMode {
                mode: Mutex::new(if m.mode == "mock" {
                    Mode::Mock
                } else {
                    Mode::Real
                }),
                refuse_mock: m.refuse_mock,
            }) as Arc<dyn ModeControl>
        }),
        factory: setup.factory.map(|f| {
            Arc::new(FakeFactory {
                configured: f.configured,
                probes: f.probes,
            }) as Arc<dyn Factory>
        }),
    };
    let catalog = Catalog::from_registry()
        .expect("catalog")
        .ordered(setup.services, setup.declared_order);
    let env = setup.process_env.clone();
    let provider = CredentialsProvider::new(&creds, catalog, hosts)
        .with_process_env(Arc::new(move |name| env.get(name).cloned()));
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register(credential_admin::SERVICE, Arc::new(provider));
    for (index, step) in fixture.steps.iter().enumerate() {
        let label = format!("{} step {index} ({})", fixture.case, step.action);
        let ctx = CallContext {
            caller: step.role.as_ref().map(|role| ServiceCaller {
                principal: "recorded-caller".into(),
                role: Some(role.clone()),
            }),
        };
        let result =
            integrations.call_json(credential_admin::SERVICE, &step.action, &step.params, &ctx);
        check_outcome(&label, &step.outcome, result);
    }
}

#[test]
fn every_recorded_session_replays_to_the_same_results() {
    let all = fixtures("credentials");
    assert!(
        all.len() >= 19,
        "expected the recorded credentials sessions, found {}",
        all.len()
    );
    for fixture in &all {
        play(fixture);
    }
}

#[test]
fn control_a_provider_that_drifts_from_a_recording_is_caught() {
    let mut drifted = fixture("credentials", "set-and-remove");
    if let Outcome::Ok { data } = &mut drifted.steps[0].outcome {
        data["last4"] = Value::String("9999".into());
    }
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(outcome.is_err(), "a different last4 must fail the replay");
}

#[test]
fn control_the_role_gate_follows_the_recorded_role() {
    let mut drifted = fixture("credentials", "role-gate");
    drifted.steps[2].role = Some("viewer".into());
    let outcome = std::panic::catch_unwind(|| play(&drifted));
    assert!(
        outcome.is_err(),
        "owner became viewer: the set must be refused"
    );
}

#[test]
fn an_anonymous_caller_with_no_role_source_is_refused_every_gated_action() {
    let creds = Credentials::default();
    let provider = CredentialsProvider::new(
        &creds,
        Catalog::from_registry().expect("catalog"),
        Hosts::default(),
    );
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register("credentials", Arc::new(provider));
    for action in ["set", "remove", "test", "mode", "rotate"] {
        let mut params = serde_json::Map::new();
        for name in ["service", "envVar", "value"] {
            params.insert(name.into(), "x".into());
        }
        params.insert("mode".into(), "mock".into());
        params.retain(|k, _| match action {
            "set" => ["service", "envVar", "value"].contains(&k.as_str()),
            "remove" => ["service", "envVar"].contains(&k.as_str()),
            "test" => k == "service",
            "mode" => k == "mode",
            _ => false,
        });
        let err = integrations
            .call_json("credentials", action, &params, &CallContext::anonymous())
            .unwrap_err();
        assert_eq!(err.code.as_deref(), Some("AUTH_ERROR"), "{action}");
    }
}

#[test]
fn fixtures_hold_no_real_secrets() {
    assert_no_secrets(&fixtures("credentials"));
}
