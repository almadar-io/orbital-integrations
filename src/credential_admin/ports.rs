//! What the `credentials` service reads and drives on its host: the credential store, the integrations manager
//! (mode switch and per-service configuration) and the declared-credential catalog. In TS these are process-wide
//! singletons installed at boot; here they are ports the host hands the provider.

use crate::provider::CallContext;
use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::Value;

use crate::infra::locale_compare;
use crate::mock::Mode;

/// A stored credential as the settings page may see it: never the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry {
    pub service: String,
    pub env_var: String,
    pub last4: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotateReport {
    pub rotated: u64,
    pub already_current: u64,
    pub unreadable: u64,
}

/// The tenant credential store (`CredentialStore` in TS). Errors are the messages TS would throw.
pub trait CredentialStorePort: Send + Sync {
    /// Whether a master key is present (writes and decryption).
    fn enabled(&self) -> bool;
    /// Masked entries in display order (service, then env var).
    fn entries(&self) -> Vec<StoredEntry>;
    fn resolve(&self, env_var: &str) -> Option<String>;
    fn set(&self, service: &str, env_var: &str, value: &str) -> Result<StoredEntry, String>;
    fn remove(&self, env_var: &str) -> Result<bool, String>;
    fn rotate(&self) -> Result<RotateReport, String>;
}

/// The manager's live mode switch (`IntegrationsModeControl`).
pub trait ModeControl: Send + Sync {
    fn mode(&self) -> Mode;
    fn set_mode(&self, mode: Mode) -> Result<(), String>;
}

/// What a probe call through the host's factory answered.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeReply {
    pub success: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
}

/// The manager's integration factory, as the `test` action uses it.
pub trait Factory: Send + Sync {
    fn is_configured(&self, service: &str) -> bool;
    fn execute(&self, service: &str, action: &str, params: &Value, ctx: &CallContext)
        -> ProbeReply;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    pub env_var: String,
    pub required: bool,
    pub description: String,
    pub one_of: Option<String>,
}

/// The declared credentials and backends of the services a host registers, in the order its status rows list.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// Services with a status row, in order.
    pub listed: Vec<String>,
    /// Every service's declared credentials, in the order the entries list.
    pub declared: Vec<(String, Vec<Declared>)>,
    pub backends: HashMap<String, String>,
}

/// Services that are never mocked, so never carry a status row (`unmockedServices`).
const UNMOCKED: [&str; 2] = ["credentials", "workspace"];

struct Ordered(Vec<(String, RegistryService)>);

#[derive(Deserialize, Default)]
struct RegistryService {
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    credentials: Vec<RegistryCredential>,
}

#[derive(Deserialize)]
struct RegistryCredential {
    #[serde(rename = "envVar")]
    env_var: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "oneOf")]
    one_of: Option<String>,
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Ordered;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a services map")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Ordered, A::Error> {
                let mut all = Vec::new();
                while let Some((name, service)) = map.next_entry::<String, RegistryService>()? {
                    all.push((name, service));
                }
                Ok(Ordered(all))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

impl Catalog {
    /// The catalog of the installed `@almadar/core` services registry, in registry order: every service with a
    /// production, simulated or stub backend that is not in [`UNMOCKED`].
    pub fn from_registry() -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Registry {
            services: Ordered,
        }
        let data = orbital_core::language_data::language_data().map_err(|e| e.to_string())?;
        let registry: Registry = serde_json::from_str(&data.services).map_err(|e| e.to_string())?;
        let mut catalog = Catalog::default();
        for (name, service) in registry.services.0 {
            let backend = service.backend.clone().unwrap_or_default();
            if matches!(backend.as_str(), "production" | "simulated" | "stub")
                && !UNMOCKED.contains(&name.as_str())
            {
                catalog.listed.push(name.clone());
            }
            catalog.backends.insert(name.clone(), backend);
            catalog.declared.push((
                name,
                service
                    .credentials
                    .into_iter()
                    .map(|c| Declared {
                        env_var: c.env_var,
                        required: c.required,
                        description: c.description,
                        one_of: c.one_of,
                    })
                    .collect(),
            ));
        }
        Ok(catalog)
    }

    /// This catalog with its listing and declaration order replaced (a host whose own contract orders them).
    pub fn ordered(mut self, listed: Vec<String>, declared_order: Vec<String>) -> Self {
        let mut remaining = std::mem::take(&mut self.declared);
        let mut declared = Vec::new();
        for name in declared_order {
            if let Some(at) = remaining.iter().position(|(n, _)| *n == name) {
                declared.push(remaining.remove(at));
            }
        }
        self.declared = declared;
        self.listed = listed;
        self
    }

    pub fn declared_for(&self, service: &str) -> &[Declared] {
        self.declared
            .iter()
            .find(|(name, _)| name == service)
            .map_or(&[], |(_, list)| list.as_slice())
    }
}

/// A store that keeps its values in memory, unencrypted: for hosts that have no persistence adapter and for
/// replaying recorded sessions. The encrypted store of `@almadar/integrations` (AES-256-GCM rows over a
/// `PersistenceAdapter`) is not ported.
pub struct MemoryCredentialStore {
    enabled: bool,
    rows: Mutex<Vec<(String, String, String)>>,
}

impl MemoryCredentialStore {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            rows: Mutex::new(Vec::new()),
        }
    }

    pub fn seed(&self, service: &str, env_var: &str, value: &str) {
        let mut rows = self.rows.lock().expect("store");
        rows.retain(|(_, e, _)| e != env_var);
        rows.push((service.to_string(), env_var.to_string(), value.to_string()));
    }
}

fn last4(value: &str) -> String {
    let units: Vec<u16> = value.encode_utf16().collect();
    String::from_utf16_lossy(&units[units.len().saturating_sub(4)..])
}

impl CredentialStorePort for MemoryCredentialStore {
    fn enabled(&self) -> bool {
        self.enabled
    }

    fn entries(&self) -> Vec<StoredEntry> {
        let mut entries: Vec<StoredEntry> = self
            .rows
            .lock()
            .expect("store")
            .iter()
            .map(|(service, env_var, value)| StoredEntry {
                service: service.clone(),
                env_var: env_var.clone(),
                last4: last4(value),
            })
            .collect();
        entries.sort_by(|a, b| {
            locale_compare(&a.service, &b.service)
                .then_with(|| locale_compare(&a.env_var, &b.env_var))
        });
        entries
    }

    fn resolve(&self, env_var: &str) -> Option<String> {
        self.rows
            .lock()
            .expect("store")
            .iter()
            .find(|(_, e, _)| e == env_var)
            .map(|(_, _, v)| v.clone())
    }

    fn set(&self, service: &str, env_var: &str, value: &str) -> Result<StoredEntry, String> {
        if !self.enabled {
            return Err(
                "ALMADAR_CREDENTIAL_MASTER_KEY is not configured \u{2014} cannot store credentials"
                    .into(),
            );
        }
        self.seed(service, env_var, value);
        Ok(StoredEntry {
            service: service.to_string(),
            env_var: env_var.to_string(),
            last4: last4(value),
        })
    }

    fn remove(&self, env_var: &str) -> Result<bool, String> {
        let mut rows = self.rows.lock().expect("store");
        let before = rows.len();
        rows.retain(|(_, e, _)| e != env_var);
        Ok(rows.len() != before)
    }

    fn rotate(&self) -> Result<RotateReport, String> {
        if !self.enabled {
            return Err(
                "ALMADAR_CREDENTIAL_MASTER_KEY is not configured \u{2014} cannot rotate".into(),
            );
        }
        Ok(RotateReport {
            rotated: 0,
            already_current: self.rows.lock().expect("store").len() as u64,
            unreadable: 0,
        })
    }
}

pub(super) fn last4_of(value: &str) -> String {
    last4(value)
}
