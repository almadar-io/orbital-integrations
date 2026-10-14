//! Twin parity: the `apikeys` provider mints random keys, so its recorded sessions
//! (`fixtures/apikeys/*.json`, from `__tests__/apikeys-recorded-exchanges.test.ts`) are replayed by shape and by
//! hash: the Rust scrypt must reproduce every hash the TS provider recorded, and a Rust mint must verify the
//! same way. Errors replay exactly.

mod infra_support;

use orbital_integrations::CallContext;
use std::sync::Arc;

use infra_support::{check_outcome, fixture, fixtures, Outcome};
use orbital_integrations::apikeys::{self, hash_api_key, ApiKeysProvider, API_KEY_PREFIX};
use orbital_integrations::{Credentials, Integrations};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Vectors {
    vectors: Vec<Vector>,
    #[serde(rename = "constructorError")]
    constructor_error: String,
}

#[derive(Deserialize)]
struct Vector {
    salt: String,
    plaintext: String,
    #[serde(rename = "keyHash")]
    key_hash: String,
    #[serde(rename = "lastFour")]
    last_four: String,
}

fn vectors() -> Vectors {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/apikeys/hash-vectors.json");
    serde_json::from_str(&infra_support::expand_placeholders(
        std::fs::read_to_string(path).expect("hash-vectors.json"),
    ))
    .expect("vectors")
}

fn rig(fixture: &infra_support::Fixture) -> Integrations {
    let creds = Credentials::from_env(fixture.env.clone());
    let mut integrations = Integrations::new(&creds).expect("mode");
    apikeys::register(&mut integrations, &creds);
    integrations
}

#[test]
fn every_recorded_hash_is_reproduced() {
    let all = vectors();
    assert!(all.vectors.len() >= 4);
    for v in &all.vectors {
        assert_eq!(
            hash_api_key(&v.plaintext, &v.salt).expect("hash"),
            v.key_hash,
            "{}",
            v.plaintext
        );
        let tail: String = v
            .plaintext
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert_eq!(tail, v.last_four);
    }
}

#[test]
fn control_a_different_salt_changes_the_hash() {
    let v = &vectors().vectors[0];
    assert_ne!(
        hash_api_key(&v.plaintext, "ff00").expect("hash"),
        v.key_hash
    );
}

#[test]
fn every_recorded_mint_verifies_and_every_error_replays() {
    let mut mints = 0;
    for fixture in fixtures("apikeys").iter().filter(|f| !f.steps.is_empty()) {
        let integrations = rig(fixture);
        let salt = fixture.env.get("ALMADAR_API_KEY_SALT").expect("salt");
        for step in &fixture.steps {
            let result = integrations.call_json(
                "apikeys",
                &step.action,
                &step.params,
                &CallContext::anonymous(),
            );
            match &step.outcome {
                Outcome::Ok { data } => {
                    let recorded_hash = data["keyHash"].as_str().expect("keyHash");
                    let plaintext = data["plaintext"].as_str().expect("plaintext");
                    assert_eq!(hash_api_key(plaintext, salt).expect("hash"), recorded_hash);
                    let minted = result.expect("mint");
                    let new_plaintext = minted["plaintext"].as_str().expect("plaintext");
                    assert!(new_plaintext.starts_with(API_KEY_PREFIX));
                    assert_eq!(
                        new_plaintext.len(),
                        data["plaintext"].as_str().expect("p").len()
                    );
                    assert_eq!(
                        minted["keyHash"].as_str().expect("hash"),
                        hash_api_key(new_plaintext, salt).expect("hash")
                    );
                    mints += 1;
                }
                outcome => check_outcome(&fixture.case, outcome, result),
            }
        }
    }
    assert!(mints >= 3, "expected the recorded mints, found {mints}");
}

#[test]
fn fixed_entropy_mints_a_fixed_key() {
    let f = fixture("apikeys", "mint");
    let creds = Credentials::from_env(f.env.clone());
    let provider = ApiKeysProvider::new(&creds)
        .expect("provider")
        .with_entropy(Arc::new(|| [0xab; 32]));
    let mut integrations = Integrations::new(&creds).expect("mode");
    integrations.register("apikeys", Arc::new(provider));
    let minted: Value = integrations
        .call_json(
            "apikeys",
            "mint",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .expect("mint");
    assert_eq!(minted["plaintext"], format!("sk_live_{}", "ab".repeat(32)));
    let v = &vectors().vectors[0];
    assert_eq!(minted["keyHash"], v.key_hash.as_str());
}

#[test]
fn an_unset_salt_is_unconfigured_and_the_provider_reports_the_ts_message() {
    let none = Credentials::default();
    let mut integrations = Integrations::new(&none).expect("mode");
    apikeys::register(&mut integrations, &none);
    let err = integrations
        .call_json(
            "apikeys",
            "mint",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .unwrap_err();
    assert_eq!(err.code.as_deref(), Some("NOT_CONFIGURED"));
    assert!(!apikeys::is_configured(&none));
    assert_eq!(
        ApiKeysProvider::new(&none)
            .err()
            .expect("constructor error")
            .message,
        vectors().constructor_error
    );
}
