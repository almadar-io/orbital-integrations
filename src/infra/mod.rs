//! What the infrastructure providers (`storage`, `redis`, `queue`, `docker`, `otel`, ...) share: an injectable
//! clock and random source (so a recorded session replays exactly), the JavaScript number and date formatting
//! their results depend on, and the "configured from declared credentials" rule of `RuntimeIntegrationManager`.

mod collate;
mod js;

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};

use orbital_core::language::registry::baked_service_credentials;
use serde_json::{Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::wallclock::system_clock;

pub use self::collate::locale_compare;
pub use self::js::{number_to_radix_string, parse_iso_ms};

/// `Math.random()`: a float in `[0, 1)`.
pub trait Rand: Send + Sync {
    fn unit(&self) -> f64;
}

/// Process-entropy seeded generator (std's `RandomState` keys), good enough for ids; a host wanting more injects a `Rand`.
pub struct SystemRand {
    state: AtomicU64,
}

impl SystemRand {
    pub fn new() -> Self {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(system_clock()() as u64);
        Self {
            state: AtomicU64::new(hasher.finish() | 1),
        }
    }
}

impl Default for SystemRand {
    fn default() -> Self {
        Self::new()
    }
}

impl Rand for SystemRand {
    fn unit(&self) -> f64 {
        // splitmix64 over an atomic counter.
        let mut z = self
            .state
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// `Math.floor(Math.random() * 256).toString(16).padStart(2, '0')` repeated `bytes` times.
pub fn random_hex_id(rand: &dyn Rand, bytes: usize) -> String {
    (0..bytes)
        .map(|_| format!("{:02x}", (rand.unit() * 256.0).floor() as u8))
        .collect()
}

/// Whether the credentials configure `service` the way `RuntimeIntegrationManager.configureFromDeclaredCredentials`
/// does: every required declared credential present.
pub fn configured_from_declared(creds: &Credentials, service: &str) -> bool {
    baked_service_credentials()
        .get(service)
        .is_none_or(|declared| {
            declared
                .iter()
                .all(|c| !c.required || creds.get(&c.env_var).is_some())
        })
}

pub fn str_param<'a>(
    params: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, IntegrationError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| IntegrationError::plain(format!("{name} must be a string")))
}

pub fn opt_str<'a>(params: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Value::as_str)
}

pub fn opt_f64(params: &Map<String, Value>, name: &str) -> Option<f64> {
    params.get(name).and_then(Value::as_f64)
}

pub fn unknown_action(service: &str, action: &str) -> IntegrationError {
    IntegrationError::plain(format!("Unknown {service} action: {action}"))
}

/// JavaScript truthiness of an optional param (`undefined` and `null` are falsy).
pub fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}
