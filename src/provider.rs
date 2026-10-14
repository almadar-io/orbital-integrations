//! The registry of providers and the kernel-facing `ServiceHandler`.

use std::collections::HashMap;
use std::sync::Arc;

use orbital_core::effects::{ServiceCaller, ServiceHandler};
use orbital_core::language::registry::baked_service_credentials;
use orbital_core::Value;
use serde_json::{Map, Value as Json};

use crate::contract;
use crate::credentials::Credentials;
use crate::error::{IntegrationError, NOT_CONFIGURED};
use crate::json::{from_json, to_json};
use crate::mock::{mode_from, CoreMockResponder, MockResponder, Mode};

/// Services mock mode never replaces: the credentials control plane (it reports and switches the mode) and the
/// host's own workspaces (local disk the program host writes into). Mirrors `unmockedServices` in the TS contract
/// (`packages/almadar-integrations/src/contracts.ts`), which is a hard-coded set there too.
pub const UNMOCKED_SERVICES: [&str; 2] = ["credentials", "workspace"];

/// What a call carries besides its params: the twin of `IntegrationCallContext`. `caller` is the dispatch's
/// `@user` (`principal` = `context.principal`, `role` = `context.role`); `None` is an anonymous call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallContext {
    pub caller: Option<ServiceCaller>,
}

impl CallContext {
    pub fn anonymous() -> Self {
        Self::default()
    }

    pub fn of(caller: Option<&ServiceCaller>) -> Self {
        Self {
            caller: caller.cloned(),
        }
    }

    pub fn signed_in(principal: &str, role: Option<&str>) -> Self {
        Self {
            caller: Some(ServiceCaller {
                principal: principal.to_string(),
                role: role.map(str::to_string),
            }),
        }
    }

    /// `context.principal`.
    pub fn principal(&self) -> Option<&str> {
        self.caller.as_ref().map(|c| c.principal.as_str())
    }

    /// `context.role`.
    pub fn role(&self) -> Option<&str> {
        self.caller.as_ref().and_then(|c| c.role.as_deref())
    }
}

/// One service's implementation: the twin of an `@almadar/integrations` integration class.
pub trait Provider: Send + Sync {
    /// Checks that run before the params are validated against the contract. TS integrations that gate an
    /// action on the caller (credentials) do so ahead of `validateParams`, so a refused caller never learns
    /// how its params were wrong.
    fn admit(&self, _action: &str, _ctx: &CallContext) -> Result<(), IntegrationError> {
        Ok(())
    }

    /// Run `action`. Params are already checked against the contract.
    fn call(
        &self,
        action: &str,
        params: &Map<String, Json>,
        ctx: &CallContext,
    ) -> Result<Json, IntegrationError>;
}

enum Slot {
    Ready(Arc<dyn Provider>),
    Failed(IntegrationError),
    NotConfigured,
}

/// Every registered provider behind the kernel's `call-service`.
pub struct Integrations {
    slots: HashMap<String, Slot>,
    mode: Mode,
    mock: Arc<dyn MockResponder>,
}

impl Integrations {
    /// A registry whose mode comes from the credentials (`ALMADAR_INTEGRATIONS_MODE` / `USE_MOCK_DATA`).
    pub fn new(creds: &Credentials) -> Result<Self, IntegrationError> {
        Ok(Self {
            slots: HashMap::new(),
            mode: mode_from(creds)?,
            mock: Arc::new(CoreMockResponder),
        })
    }

    pub fn with_mock_responder(mut self, responder: Arc<dyn MockResponder>) -> Self {
        self.mock = responder;
        self
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Register a configured provider.
    pub fn register(&mut self, service: &str, provider: Arc<dyn Provider>) {
        self.slots
            .insert(service.to_string(), Slot::Ready(provider));
    }

    /// Register a service whose construction failed; every call reports that failure.
    pub fn register_failed(&mut self, service: &str, error: IntegrationError) {
        self.slots.insert(service.to_string(), Slot::Failed(error));
    }

    /// Register a service the credentials leave unconfigured.
    pub fn register_unconfigured(&mut self, service: &str) {
        self.slots.insert(service.to_string(), Slot::NotConfigured);
    }

    /// The entry point both the `ServiceHandler` and the parity tests use.
    pub fn call_json(
        &self,
        service: &str,
        action: &str,
        params: &Map<String, Json>,
        ctx: &CallContext,
    ) -> Result<Json, IntegrationError> {
        if self.mode == Mode::Mock && !UNMOCKED_SERVICES.contains(&service) {
            return self.mock.respond(service, action, params);
        }
        let provider = match self.slots.get(service) {
            Some(Slot::Ready(p)) => p,
            Some(Slot::Failed(e)) => return Err(e.clone()),
            Some(Slot::NotConfigured) => return Err(not_configured(service)),
            None => {
                return Err(IntegrationError::plain(format!(
                    "Integration '{service}' has no Rust provider in this host"
                )))
            }
        };
        provider.admit(action, ctx)?;
        contract::validate(service, action, params)?;
        provider.call(action, params, ctx)
    }
}

fn not_configured(service: &str) -> IntegrationError {
    let vars: Vec<&str> = baked_service_credentials()
        .get(service)
        .map(|list| list.iter().map(|c| c.env_var.as_str()).collect())
        .unwrap_or_default();
    let needs = if vars.is_empty() {
        String::new()
    } else {
        format!(" Set {}.", vars.join(" / "))
    };
    IntegrationError::coded(
        NOT_CONFIGURED,
        format!("Integration not configured: {service}.{needs}"),
    )
}

impl ServiceHandler for Integrations {
    fn call(
        &self,
        service: &str,
        method: &str,
        args: &Value,
        _key: Option<&str>,
        caller: Option<&ServiceCaller>,
    ) -> Result<Value, String> {
        let params = match to_json(args) {
            Json::Object(map) => map,
            Json::Null => Map::new(),
            other => {
                return Err(format!(
                    "{service}.{method}: params must be an object, got {other}"
                ))
            }
        };
        self.call_json(service, method, &params, &CallContext::of(caller))
            .map(|out| from_json(&out))
            .map_err(|e| e.message)
    }

    /// Calls run to completion on the caller's thread, so there is nothing in flight to abort.
    fn cancel(&self, _key: &str) -> Result<(), String> {
        Ok(())
    }
}
