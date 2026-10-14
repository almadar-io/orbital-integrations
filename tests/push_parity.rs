//! Twin parity for the `push` provider: every case `__tests__/push-recorded-exchanges.test.ts` recorded
//! (`fixtures/push/*.json`) replays to the same result or error. The recording pins the SDK's ephemeral key and salt
//! and freezes its clock, so the encrypted body is compared byte for byte; the VAPID JWT (randomized signature in TS)
//! is verified against the VAPID public key, and the body is decrypted with the receiver's key.

mod l13d_common;

use orbital_integrations::CallContext;
use std::sync::Arc;

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use l13d_common::*;
use orbital_integrations::hmacsha256::hmac_sha256;
use orbital_integrations::push::{self, Entropy, PushProvider};
use orbital_integrations::{Credentials, IntegrationError, Provider, Transport};
use p256::ecdh::diffie_hellman;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use serde_json::Value;

const HEADERS: [&str; 4] = ["ttl", "urgency", "content-encoding", "content-length"];

struct Pinned {
    private_key: [u8; 32],
    salt: [u8; 16],
}

impl Entropy for Pinned {
    fn ephemeral_private_key(&self) -> [u8; 32] {
        self.private_key
    }
    fn salt(&self) -> [u8; 16] {
        self.salt
    }
}

fn text<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut at = value;
    for key in path {
        at = at
            .get(*key)
            .unwrap_or_else(|| panic!("fixture extra misses {key}"));
    }
    at.as_str().expect("string")
}

fn pinned(fixture: &Fixture) -> Pinned {
    let extra = Value::Object(fixture.extra.clone());
    let key = URL_SAFE_NO_PAD
        .decode(text(&extra, &["entropy", "ephemeralPrivateKey"]))
        .expect("key");
    let salt = URL_SAFE_NO_PAD
        .decode(text(&extra, &["entropy", "salt"]))
        .expect("salt");
    Pinned {
        private_key: key.try_into().expect("32 bytes"),
        salt: salt.try_into().expect("16 bytes"),
    }
}

fn build(
    creds: &Credentials,
    transport: Arc<dyn Transport>,
    fixture: &Fixture,
) -> Result<Arc<dyn Provider>, IntegrationError> {
    Ok(Arc::new(
        PushProvider::new(creds, transport)
            .with_entropy(Arc::new(pinned(fixture)))
            .with_clock(clock_of(fixture)),
    ))
}

#[test]
fn every_recorded_case_replays_to_the_same_result() {
    replay_all(push::SERVICE, 41, &HEADERS, build);
}

#[test]
fn control_a_different_payload_changes_the_ciphertext_and_is_caught() {
    let mut drifted = fixture(push::SERVICE, "send");
    drifted
        .params
        .insert("title".to_string(), Value::String("Other".to_string()));
    let outcome = std::panic::catch_unwind(|| run(&drifted, &HEADERS, build));
    assert!(outcome.is_err(), "a different title must fail the replay");
}

fn decrypt(body: &[u8], receiver_private: &str, receiver_auth: &str) -> Vec<u8> {
    let salt = &body[..16];
    let record_size = u32::from_be_bytes([body[16], body[17], body[18], body[19]]) as usize;
    let key_len = body[20] as usize;
    let sender_public = &body[21..21 + key_len];
    let records = &body[21 + key_len..];

    let receiver_secret =
        SecretKey::from_slice(&URL_SAFE_NO_PAD.decode(receiver_private).expect("priv"))
            .expect("scalar");
    let receiver_public = receiver_secret.public_key().to_encoded_point(false);
    let sender = PublicKey::from_sec1_bytes(sender_public).expect("sender key");
    let shared = diffie_hellman(receiver_secret.to_nonzero_scalar(), sender.as_affine());

    let expand = |prk: &[u8], info: &[u8], len: usize| {
        hmac_sha256(prk, &[info, &[1u8]].concat())[..len].to_vec()
    };
    let auth = URL_SAFE_NO_PAD.decode(receiver_auth).expect("auth");
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(receiver_public.as_bytes());
    key_info.extend_from_slice(sender_public);
    let secret = expand(
        &hmac_sha256(&auth, shared.raw_secret_bytes().as_slice()),
        &key_info,
        32,
    );
    let prk = hmac_sha256(salt, &secret);
    let key = expand(&prk, b"Content-Encoding: aes128gcm\0", 16);
    let nonce_base = expand(&prk, b"Content-Encoding: nonce\0", 12);
    let cipher = Aes128Gcm::new(GenericArray::from_slice(&key));

    let mut plain = Vec::new();
    for (counter, record) in records.chunks(record_size).enumerate() {
        let mut nonce = nonce_base.clone();
        let tail = u64::from_be_bytes([
            0, 0, nonce[6], nonce[7], nonce[8], nonce[9], nonce[10], nonce[11],
        ]) ^ counter as u64;
        nonce[6..12].copy_from_slice(&tail.to_be_bytes()[2..8]);
        let mut block = cipher
            .decrypt(GenericArray::from_slice(&nonce), record)
            .expect("authentic record");
        let delimiter = block.iter().rposition(|b| *b != 0).expect("delimiter");
        block.truncate(delimiter);
        plain.extend(block);
    }
    plain
}

#[test]
fn the_recorded_ciphertext_decrypts_to_the_payload_and_the_rust_one_is_identical() {
    for case in [
        "send",
        "send-unicode-payload",
        "send-large-payload-spans-records",
        "send-empty-url-and-icon-are-dropped",
    ] {
        let f = fixture(push::SERVICE, case);
        let extra = Value::Object(f.extra.clone());
        let recorded = base64::engine::general_purpose::STANDARD
            .decode(text(&extra, &["bodyBase64"]))
            .expect("body");
        let plain = decrypt(
            &recorded,
            text(&extra, &["receiver", "privateKey"]),
            text(&extra, &["receiver", "auth"]),
        );
        let payload: Value = serde_json::from_slice(&plain).expect("payload json");
        assert_eq!(payload["title"], f.params["title"], "{case}");
        assert_eq!(payload["body"], f.params["body"], "{case}");
        assert_eq!(
            payload.get("url").is_some(),
            f.params
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| !u.is_empty()),
            "{case}"
        );

        let (result, replay) = run(&f, &HEADERS, build);
        check(&f, result, &replay);
        let sent = replay.sent.lock().expect("sent");
        assert_eq!(sent.len(), 1, "{case}");
    }
}

#[test]
fn the_vapid_authorization_verifies_against_the_public_key_and_carries_the_audience_expiry_and_subject(
) {
    for (case, audience) in [
        ("send", "https://push.example.test"),
        (
            "send-endpoint-with-port-and-query",
            "https://push.example.test:8443",
        ),
        ("send-to-a-fcm-endpoint", "https://fcm.googleapis.com"),
        ("vapid-subject-https-url", "https://push.example.test"),
    ] {
        let f = fixture(push::SERVICE, case);
        let extra = Value::Object(f.extra.clone());
        let (_, replay) = run(&f, &HEADERS, build);
        let sent = replay.sent.lock().expect("sent");
        let auth = &sent[0]
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .expect("authorization")
            .1;
        let rest = auth.strip_prefix("vapid t=").expect("vapid scheme");
        let (jwt, key) = rest.split_once(", k=").expect("key part");
        assert_eq!(key, text(&extra, &["vapid", "publicKey"]), "{case}");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "{case}");
        let header: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).expect("header"))
                .expect("json");
        assert_eq!(header, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("claims"))
                .expect("json");
        assert_eq!(claims["aud"], audience, "{case}");
        assert_eq!(
            claims["exp"],
            1_780_315_200_i64 + 43_200,
            "{case}: 2026-06-01T12:00:00Z + 12h"
        );
        assert_eq!(claims["sub"], f.env["VAPID_SUBJECT"], "{case}");
        let verifier = VerifyingKey::from_sec1_bytes(&URL_SAFE_NO_PAD.decode(key).expect("pub"))
            .expect("vapid key");
        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).expect("sig"))
            .expect("signature");
        verifier
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .unwrap_or_else(|e| panic!("{case}: bad signature {e}"));
    }
}

#[test]
fn a_gcm_endpoint_is_sent_without_authorization() {
    let f = fixture(push::SERVICE, "send-to-a-gcm-endpoint-skips-vapid");
    let (_, replay) = run(&f, &HEADERS, build);
    let sent = replay.sent.lock().expect("sent");
    assert!(sent[0].headers.iter().all(|(k, _)| k != "Authorization"));
}

#[test]
fn an_unconfigured_service_is_reported_when_registered() {
    use orbital_integrations::error::NOT_CONFIGURED;
    use orbital_integrations::Integrations;
    let creds = Credentials::default();
    let mut integrations = Integrations::new(&creds).expect("mode");
    let transport: Arc<dyn Transport> = Replay::new(&fixture(push::SERVICE, "send"), &HEADERS);
    push::register(&mut integrations, &creds, transport);
    let err = integrations
        .call_json(
            push::SERVICE,
            "send",
            &serde_json::Map::new(),
            &CallContext::anonymous(),
        )
        .expect_err("unconfigured");
    assert_eq!(err.code.as_deref(), Some(NOT_CONFIGURED));
}
