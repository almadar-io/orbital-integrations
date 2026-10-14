//! The `credentials` service: the Rust twin of `CredentialsIntegration` (`@almadar/integrations`
//! `integrations/credentials`), the hosted credential store's service surface. Actions: `list`, `set`, `remove`,
//! `test`, `mode`, `rotate`. The store, the manager's mode switch and factory and the declared-credential catalog
//! come in as ports (see [`ports`]).

mod ports;

use std::sync::Arc;

use serde_json::{json, Map, Value};

pub use self::ports::{
    Catalog, CredentialStorePort, Declared, Factory, MemoryCredentialStore, ModeControl,
    ProbeReply, RotateReport, StoredEntry,
};
use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::infra::{opt_str, str_param, unknown_action};
use crate::mock::Mode;
use crate::provider::{CallContext, Integrations, Provider};

pub const SERVICE: &str = "credentials";

const AUTH_ERROR: &str = "AUTH_ERROR";
const ROLE_GATED: [&str; 5] = ["set", "remove", "test", "rotate", "mode"];
const DEFAULT_ADMIN_ROLES: [&str; 2] = ["admin", "owner"];
const FIRESTORE_SERVICES: [&str; 1] = ["knowledge"];
const FIRESTORE_NEEDS: &str = "FIRESTORE_EMULATOR_HOST (start the Firebase emulators), or FIREBASE_PROJECT_ID / FIREBASE_SERVICE_ACCOUNT_PATH";

/// The side-effect-free "Test connection" probes (`serviceProbes` in the TS contract).
const PROBES: [(&str, &str, &str); 3] = [
    ("calendar", "listEvents", r#"{"maxResults":1}"#),
    ("drive", "listFiles", r#"{"maxResults":1}"#),
    ("metaAds", "listCampaigns", "{}"),
];

type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct CredentialsProvider {
    config_roles: Option<String>,
    config_source: Option<String>,
    process_env: EnvLookup,
    catalog: Catalog,
    store: Option<Arc<dyn CredentialStorePort>>,
    mode: Option<Arc<dyn ModeControl>>,
    factory: Option<Arc<dyn Factory>>,
    env_mode: Mode,
}

/// The host's side of the service.
#[derive(Default)]
pub struct Hosts {
    pub store: Option<Arc<dyn CredentialStorePort>>,
    pub mode: Option<Arc<dyn ModeControl>>,
    pub factory: Option<Arc<dyn Factory>>,
}

/// The credentials service is always configured; it degrades honestly when a port is absent.
pub fn register(integrations: &mut Integrations, creds: &Credentials, hosts: Hosts) {
    match Catalog::from_registry() {
        Ok(catalog) => {
            let provider = CredentialsProvider::new(creds, catalog, hosts);
            integrations.register(SERVICE, Arc::new(provider));
        }
        Err(reason) => integrations.register_failed(SERVICE, IntegrationError::plain(reason)),
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Mock => "mock",
        Mode::Real => "real",
    }
}

impl CredentialsProvider {
    pub fn new(creds: &Credentials, catalog: Catalog, hosts: Hosts) -> Self {
        Self {
            config_roles: creds
                .get("ALMADAR_CREDENTIAL_ADMIN_ROLES")
                .map(str::to_string),
            config_source: creds.get("ALMADAR_CREDENTIALS_SOURCE").map(str::to_string),
            process_env: Arc::new(|name| std::env::var(name).ok()),
            catalog,
            store: hosts.store,
            mode: hosts.mode,
            factory: hosts.factory,
            env_mode: Mode::Real,
        }
    }

    /// Where the process env is read from (`process.env` in TS; credential values and the mode switch).
    pub fn with_process_env(mut self, lookup: EnvLookup) -> Self {
        self.env_mode = if lookup("ALMADAR_INTEGRATIONS_MODE").as_deref() == Some("mock")
            || lookup("USE_MOCK_DATA").as_deref() == Some("true")
        {
            Mode::Mock
        } else {
            Mode::Real
        };
        self.process_env = lookup;
        self
    }

    fn env(&self, name: &str) -> Option<String> {
        (self.process_env)(name).filter(|v| !v.is_empty())
    }

    fn admin_roles(&self) -> Vec<String> {
        let raw = self
            .config_roles
            .clone()
            .or_else(|| self.env("ALMADAR_CREDENTIAL_ADMIN_ROLES"))
            .unwrap_or_default();
        let roles: Vec<String> = raw
            .split(',')
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect();
        if roles.is_empty() {
            DEFAULT_ADMIN_ROLES
                .iter()
                .map(|r| (*r).to_string())
                .collect()
        } else {
            roles
        }
    }

    fn store_first(&self) -> bool {
        self.config_source
            .clone()
            .or_else(|| self.env("ALMADAR_CREDENTIALS_SOURCE"))
            .as_deref()
            == Some("store")
    }

    fn assert_settable(&self, service: &str, env_var: &str) -> Result<(), IntegrationError> {
        if service == "database" {
            let well_formed = env_var
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
                && env_var
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
            if !well_formed {
                return Err(IntegrationError::plain(format!(
                    "\"{env_var}\" is not a well-formed connection reference (expected an env-var name)"
                )));
            }
            return Ok(());
        }
        let declared = self.catalog.declared_for(service);
        if declared.is_empty() {
            return Err(IntegrationError::plain(format!(
                "Service \"{service}\" declares no credentials"
            )));
        }
        if !declared.iter().any(|c| c.env_var == env_var) {
            let valid: Vec<&str> = declared.iter().map(|c| c.env_var.as_str()).collect();
            return Err(IntegrationError::plain(format!(
                "\"{env_var}\" is not a declared credential of \"{service}\" (declared: {})",
                valid.join(", ")
            )));
        }
        Ok(())
    }

    fn resolve_ref(&self, reference: &str) -> Option<String> {
        self.store
            .as_ref()
            .and_then(|s| s.resolve(reference))
            .or_else(|| (self.process_env)(reference))
    }

    fn mode_now(&self) -> Mode {
        self.mode.as_ref().map_or(self.env_mode, |m| m.mode())
    }

    fn credential_needs(&self, service: &str) -> String {
        let declared = self.catalog.declared_for(service);
        let mut parts: Vec<String> = declared
            .iter()
            .filter(|d| d.required)
            .map(|d| d.env_var.clone())
            .collect();
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for d in declared {
            if let Some(group) = &d.one_of {
                match groups.iter_mut().find(|(g, _)| g == group) {
                    Some((_, vars)) => vars.push(d.env_var.clone()),
                    None => groups.push((group.clone(), vec![d.env_var.clone()])),
                }
            }
        }
        parts.extend(
            groups
                .iter()
                .map(|(_, vars)| format!("one of {}", vars.join(" / "))),
        );
        parts.join(", ")
    }

    fn firestore_target_is_emulator(&self) -> bool {
        self.env("FIRESTORE_EMULATOR_HOST").is_some()
    }

    fn list(&self, filter: Option<&str>) -> Value {
        let stored_entries = self.store.as_ref().map(|s| s.entries()).unwrap_or_default();
        let mut stored: Vec<Option<StoredEntry>> = stored_entries.into_iter().map(Some).collect();
        let mut entries = Vec::new();
        for (service, declared) in &self.catalog.declared {
            if filter.is_some_and(|f| f != service) {
                continue;
            }
            for d in declared {
                let from_store = stored
                    .iter_mut()
                    .find(|e| {
                        e.as_ref()
                            .is_some_and(|e| e.service == *service && e.env_var == d.env_var)
                    })
                    .and_then(Option::take);
                let from_env = if self.store_first() {
                    None
                } else {
                    self.env(&d.env_var)
                };
                let source = if from_store.is_some() {
                    "store"
                } else if from_env.is_some() {
                    "env"
                } else {
                    "none"
                };
                let last4 = match (&from_store, &from_env) {
                    (Some(entry), _) => entry.last4.clone(),
                    (None, Some(value)) => ports::last4_of(value),
                    (None, None) => String::new(),
                };
                entries.push(json!({
                    "id": format!("{service}/{}", d.env_var),
                    "service": service,
                    "envVar": d.env_var,
                    "required": d.required,
                    "description": d.description,
                    "configured": source != "none",
                    "source": source,
                    "last4": last4,
                }));
            }
        }
        for entry in stored.into_iter().flatten() {
            if filter.is_some_and(|f| f != entry.service) {
                continue;
            }
            entries.push(json!({
                "id": format!("{}/{}", entry.service, entry.env_var),
                "service": entry.service,
                "envVar": entry.env_var,
                "required": false,
                "description": "Stored connection reference",
                "configured": true,
                "source": "store",
                "last4": entry.last4,
            }));
        }
        let mode = self.mode_now();
        let mut services = Vec::new();
        for service in &self.catalog.listed {
            if filter.is_some_and(|f| f != service) {
                continue;
            }
            let configured = self
                .factory
                .as_ref()
                .is_some_and(|f| f.is_configured(service));
            let mocked = mode == Mode::Mock && configured;
            let on_firestore = FIRESTORE_SERVICES.contains(&service.as_str());
            let emulated = on_firestore && self.firestore_target_is_emulator();
            let status = if mocked {
                "mocked"
            } else if !configured {
                "not configured"
            } else if emulated {
                "emulated"
            } else {
                "real"
            };
            let needs = if configured {
                String::new()
            } else if on_firestore {
                FIRESTORE_NEEDS.to_string()
            } else {
                self.credential_needs(service)
            };
            services.push(json!({
                "id": service,
                "service": service,
                "configured": configured,
                "mocked": mocked,
                "status": status,
                "backend": self.catalog.backends.get(service).cloned().unwrap_or_default(),
                "needs": needs,
            }));
        }
        json!({
            "enabled": self.store.as_ref().is_some_and(|s| s.enabled()),
            "mode": mode_name(mode),
            "services": services,
            "entries": entries,
        })
    }

    fn no_store(suffix: &str) -> IntegrationError {
        IntegrationError::plain(format!(
            "No credential store is installed on this host{suffix}"
        ))
    }

    fn test(&self, service: &str, ctx: &CallContext) -> Value {
        let declared = self.catalog.declared_for(service);
        let missing: Vec<String> = declared
            .iter()
            .filter(|c| c.required && self.resolve_ref(&c.env_var).is_none_or(|v| v.is_empty()))
            .map(|c| c.env_var.clone())
            .collect();
        let configured = missing.is_empty();
        let report = |probed: bool, ok: bool, message: String| json!({ "service": service, "configured": configured, "missing": missing, "probed": probed, "ok": ok, "message": message });
        if !configured {
            return report(
                false,
                false,
                format!("Missing required credentials: {}", missing.join(", ")),
            );
        }
        let probe = PROBES.iter().find(|(name, _, _)| *name == service);
        let (Some(factory), Some((_, action, params))) = (self.factory.as_ref(), probe) else {
            return report(
                false,
                true,
                "Credentials present (no live probe declared for this service)".into(),
            );
        };
        if !factory.is_configured(service) {
            return report(false, false, "Credentials present but the service is not configured on this host \u{2014} restart or re-save a credential".into());
        }
        let params: Value = serde_json::from_str(params).unwrap_or(Value::Null);
        let reply = factory.execute(service, action, &params, ctx);
        if !reply.success {
            let message = reply
                .error
                .unwrap_or_else(|| format!("Probe {action} failed"));
            return report(true, false, message);
        }
        if reply
            .data
            .as_ref()
            .is_some_and(|d| d.as_object().is_some_and(|o| o.contains_key("_mock")))
        {
            return report(
                false,
                false,
                "Probe was mock-echoed \u{2014} the service is not actually configured".into(),
            );
        }
        report(true, true, format!("Probe {action} succeeded"))
    }

    /// The role gate (`ROLE_GATED_ACTIONS` in TS): a gated action needs the caller's role in the admin list and
    /// fails closed without one.
    fn gate(&self, action: &str, ctx: &CallContext) -> Result<(), IntegrationError> {
        if !ROLE_GATED.contains(&action) {
            return Ok(());
        }
        let allowed = self.admin_roles();
        if ctx
            .role()
            .is_none_or(|r| r.is_empty() || !allowed.iter().any(|a| a == r))
        {
            return Err(IntegrationError::coded(
                AUTH_ERROR,
                format!(
                    "forbidden: \"{action}\" requires one of roles: {}",
                    allowed.join(", ")
                ),
            ));
        }
        Ok(())
    }

    fn run(
        &self,
        ctx: &CallContext,
        action: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, IntegrationError> {
        match action {
            "list" => Ok(self.list(opt_str(params, "service"))),
            "set" => {
                let (service, env_var, value) = (
                    str_param(params, "service")?,
                    str_param(params, "envVar")?,
                    str_param(params, "value")?,
                );
                let store = self.store.as_ref().ok_or_else(|| {
                    Self::no_store(" \u{2014} set credentials via the environment")
                })?;
                self.assert_settable(service, env_var)?;
                let saved = store
                    .set(service, env_var, value)
                    .map_err(IntegrationError::plain)?;
                Ok(
                    json!({ "saved": true, "service": service, "envVar": env_var, "last4": saved.last4 }),
                )
            }
            "remove" => {
                let (service, env_var) =
                    (str_param(params, "service")?, str_param(params, "envVar")?);
                let store = self.store.as_ref().ok_or_else(|| Self::no_store(""))?;
                self.assert_settable(service, env_var)?;
                let removed = store.remove(env_var).map_err(IntegrationError::plain)?;
                Ok(json!({ "removed": removed }))
            }
            "test" => Ok(self.test(str_param(params, "service")?, ctx)),
            "mode" => {
                let requested = match params.get("mode").and_then(Value::as_str) {
                    Some("mock") => Mode::Mock,
                    Some("real") => Mode::Real,
                    _ => return Err(IntegrationError::plain("mode must be \"mock\" or \"real\"")),
                };
                let control = self.mode.as_ref().ok_or_else(|| {
                    IntegrationError::plain("No integration manager is installed on this host")
                })?;
                control
                    .set_mode(requested)
                    .map_err(IntegrationError::plain)?;
                Ok(json!({ "mode": mode_name(control.mode()) }))
            }
            "rotate" => {
                let store = self.store.as_ref().ok_or_else(|| Self::no_store(""))?;
                let report = store.rotate().map_err(IntegrationError::plain)?;
                Ok(json!({
                    "rotated": report.rotated,
                    "alreadyCurrent": report.already_current,
                    "unreadable": report.unreadable,
                }))
            }
            other => Err(unknown_action("credentials", other)),
        }
    }
}

impl Provider for CredentialsProvider {
    fn admit(&self, action: &str, ctx: &CallContext) -> Result<(), IntegrationError> {
        self.gate(action, ctx)
    }

    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        self.gate(action, ctx)?;
        self.run(ctx, action, params)
    }
}
