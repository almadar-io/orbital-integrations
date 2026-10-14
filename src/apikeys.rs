//! The `apikeys` service: the Rust twin of `ApiKeysIntegration` (`@almadar/integrations` `integrations/apikeys`)
//! over `mintApiKey`/`hashApiKey` of `@almadar/auth`. One action: `mint`.

use std::sync::Arc;

use scrypt::Params;
use serde_json::{json, Map, Value};

use crate::credentials::Credentials;
use crate::error::IntegrationError;
use crate::infra::unknown_action;
use crate::provider::{CallContext, Integrations, Provider};

pub const SERVICE: &str = "apikeys";
pub const SALT_VAR: &str = "ALMADAR_API_KEY_SALT";
/// Every API key starts with this, which is how a bearer is routed to the key verifier.
pub const API_KEY_PREFIX: &str = "sk_live_";

const SCRYPT_LOG_N: u8 = 14;
const SCRYPT_R: u32 = 8;
const SCRYPT_P: u32 = 1;
const SCRYPT_KEYLEN: usize = 32;

type Entropy = Arc<dyn Fn() -> [u8; 32] + Send + Sync>;

fn hex_pairs(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() || !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `hashApiKey`: scrypt (N=16384, r=8, p=1) of the key over the hex-decoded salt, as lowercase hex.
pub fn hash_api_key(plaintext: &str, salt: &str) -> Result<String, IntegrationError> {
    let salt = hex_pairs(salt).ok_or_else(|| {
        IntegrationError::plain(
            "@almadar/auth/apikey: the API key salt must be non-empty hex (ALMADAR_API_KEY_SALT)",
        )
    })?;
    let params = Params::new(SCRYPT_LOG_N, SCRYPT_R, SCRYPT_P, SCRYPT_KEYLEN)
        .map_err(|e| IntegrationError::plain(e.to_string()))?;
    let mut out = [0u8; SCRYPT_KEYLEN];
    scrypt::scrypt(plaintext.as_bytes(), &salt, &params, &mut out)
        .map_err(|e| IntegrationError::plain(e.to_string()))?;
    Ok(hex(&out))
}

pub struct ApiKeysProvider {
    salt: String,
    entropy: Entropy,
}

/// Whether the credentials configure `apikeys` (the salt is its one required credential).
pub fn is_configured(creds: &Credentials) -> bool {
    creds.get(SALT_VAR).is_some()
}

pub fn register(integrations: &mut Integrations, creds: &Credentials) {
    match ApiKeysProvider::new(creds) {
        Ok(provider) => integrations.register(SERVICE, Arc::new(provider)),
        Err(_) => integrations.register_unconfigured(SERVICE),
    }
}

impl ApiKeysProvider {
    pub fn new(creds: &Credentials) -> Result<Self, IntegrationError> {
        let salt = creds
            .get(SALT_VAR)
            .ok_or_else(|| IntegrationError::plain("ALMADAR_API_KEY_SALT not configured"))?;
        Ok(Self {
            salt: salt.to_string(),
            entropy: Arc::new(|| {
                let mut bytes = [0u8; 32];
                getrandom::getrandom(&mut bytes).expect("operating system entropy");
                bytes
            }),
        })
    }

    /// Replace the 32 random bytes a key is made of (tests).
    pub fn with_entropy(mut self, entropy: Entropy) -> Self {
        self.entropy = entropy;
        self
    }
}

impl Provider for ApiKeysProvider {
    fn call(
        &self,
        action: &str,
        _params: &Map<String, Value>,
        _ctx: &CallContext,
    ) -> Result<Value, IntegrationError> {
        if action != "mint" {
            return Err(unknown_action("apikeys", action));
        }
        let plaintext = format!("{API_KEY_PREFIX}{}", hex(&(self.entropy)()));
        let key_hash = hash_api_key(&plaintext, &self.salt)?;
        let last_four: String = plaintext
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        Ok(json!({ "plaintext": plaintext, "keyHash": key_hash, "lastFour": last_four }))
    }
}
