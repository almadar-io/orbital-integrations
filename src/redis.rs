//! The `redis` service: the Rust twin of `RedisIntegration` (`@almadar/integrations` `integrations/redis`).
//! Like the TS provider it is an in-memory store (TTLs, locks, counters, channel names); a configured `REDIS_URL`
//! does not change that. Actions: `get`, `set`, `delete`, `lock`, `unlock`, `increment`, `expire`, `publish`, `subscribe`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::infra::{
    configured_from_declared, number_to_radix_string, opt_f64, str_param, unknown_action, Rand,
    SystemRand,
};
use crate::json::number;
use crate::provider::{CallContext, Integrations, Provider};
use crate::wallclock::{system_clock, Clock};

pub const SERVICE: &str = "redis";
const DEFAULT_LOCK_TTL_MS: f64 = 30_000.0;

struct Entry {
    value: Value,
    expires_at: Option<f64>,
}

#[derive(Default)]
struct State {
    store: HashMap<String, Entry>,
    locks: HashMap<String, String>,
    channels: HashSet<String>,
}

pub struct RedisProvider {
    state: Mutex<State>,
    clock: Clock,
    rand: Arc<dyn Rand>,
}

/// Whether the credentials configure `redis` (`REDIS_URL` is a required declared credential).
pub fn is_configured(creds: &Credentials) -> bool {
    configured_from_declared(creds, SERVICE)
}

pub fn register(integrations: &mut Integrations, creds: &Credentials) {
    if is_configured(creds) {
        integrations.register(SERVICE, Arc::new(RedisProvider::new()));
    } else {
        integrations.register_unconfigured(SERVICE);
    }
}

impl Default for RedisProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl RedisProvider {
    pub fn new() -> Self {
        Self::with_sources(system_clock(), Arc::new(SystemRand::new()))
    }

    pub fn with_sources(clock: Clock, rand: Arc<dyn Rand>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            clock,
            rand,
        }
    }

    fn now(&self) -> f64 {
        (self.clock)() as f64
    }

    fn is_alive(&self, state: &mut State, key: &str) -> bool {
        let Some(entry) = state.store.get(key) else {
            return false;
        };
        if entry.expires_at.is_some_and(|at| self.now() > at) {
            state.store.remove(key);
            return false;
        }
        true
    }

    fn lock_id(&self) -> String {
        let suffix: String = number_to_radix_string(self.rand.unit(), 36)
            .chars()
            .skip(2)
            .take(8)
            .collect();
        format!("lock_{}_{suffix}", (self.clock)())
    }
}

impl Provider for RedisProvider {
    fn call(
        &self,
        action: &str,
        params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        let mut state = self.state.lock().expect("redis state");
        let state = &mut *state;
        match action {
            "get" => {
                let key = str_param(params, "key")?;
                if !self.is_alive(state, key) {
                    return Ok(json!({ "value": null }));
                }
                let value = state
                    .store
                    .get(key)
                    .map_or(Value::Null, |e| e.value.clone());
                Ok(json!({ "value": value }))
            }
            "set" => {
                let key = str_param(params, "key")?;
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                let expires_at = opt_f64(params, "ttl")
                    .filter(|ttl| *ttl > 0.0)
                    .map(|ttl| self.now() + ttl * 1000.0);
                state
                    .store
                    .insert(key.to_string(), Entry { value, expires_at });
                Ok(json!({ "ok": true }))
            }
            "delete" => {
                let key = str_param(params, "key")?;
                Ok(json!({ "deleted": state.store.remove(key).is_some() }))
            }
            "lock" => {
                let key = str_param(params, "key")?;
                let ttl = opt_f64(params, "ttl").unwrap_or(DEFAULT_LOCK_TTL_MS);
                let lock_key = format!("__lock:{key}");
                if state.locks.contains_key(key) {
                    if self.is_alive(state, &lock_key) {
                        return Ok(json!({ "acquired": false, "lockId": "" }));
                    }
                    state.locks.remove(key);
                    state.store.remove(&lock_key);
                }
                let lock_id = self.lock_id();
                state.locks.insert(key.to_string(), lock_id.clone());
                state.store.insert(
                    lock_key,
                    Entry {
                        value: Value::String(lock_id.clone()),
                        expires_at: Some(self.now() + ttl),
                    },
                );
                Ok(json!({ "acquired": true, "lockId": lock_id }))
            }
            "unlock" => {
                let key = str_param(params, "key")?;
                let lock_id = str_param(params, "lockId")?;
                if state.locks.get(key).map(String::as_str) != Some(lock_id) {
                    return Ok(json!({ "released": false }));
                }
                state.locks.remove(key);
                state.store.remove(&format!("__lock:{key}"));
                Ok(json!({ "released": true }))
            }
            "increment" => {
                let key = str_param(params, "key")?;
                let by = opt_f64(params, "by").unwrap_or(1.0);
                let current = if self.is_alive(state, key) {
                    state
                        .store
                        .get(key)
                        .and_then(|e| e.value.as_f64())
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                let next = current + by;
                let expires_at = state.store.get(key).and_then(|e| e.expires_at);
                state.store.insert(
                    key.to_string(),
                    Entry {
                        value: number(next),
                        expires_at,
                    },
                );
                Ok(json!({ "value": number(next) }))
            }
            "expire" => {
                let key = str_param(params, "key")?;
                let ttl = opt_f64(params, "ttl").unwrap_or(0.0);
                if !self.is_alive(state, key) {
                    return Ok(json!({ "set": false }));
                }
                match state.store.get_mut(key) {
                    Some(entry) => {
                        entry.expires_at = Some(self.now() + ttl * 1000.0);
                        Ok(json!({ "set": true }))
                    }
                    None => Ok(json!({ "set": false })),
                }
            }
            "publish" => {
                str_param(params, "channel")?;
                Ok(json!({ "receivers": 0 }))
            }
            "subscribe" => {
                state
                    .channels
                    .insert(str_param(params, "channel")?.to_string());
                Ok(json!({ "subscribed": true }))
            }
            other => Err(unknown_action("redis", other)),
        }
    }
}
