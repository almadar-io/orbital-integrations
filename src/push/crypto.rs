//! The cryptography of `web-push`: RFC 8291 `aes128gcm` content encryption and the VAPID ES256 JWT (RFC 8292).

use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use p256::ecdh::diffie_hellman;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};

use crate::hmacsha256::hmac_sha256;
use crate::jswire::base64_decode_lenient;

const RECORD_SIZE: usize = 4096;
const TAG_LENGTH: usize = 16;

/// A message to surface as the SDK's error.
#[derive(Debug, PartialEq, Eq)]
pub struct EncryptError(pub String);

fn fail(message: &str) -> EncryptError {
    EncryptError(message.to_string())
}

/// The randomness one encryption consumes: the sender's ephemeral ECDH key and the 16-byte salt.
pub trait Entropy: Send + Sync {
    fn ephemeral_private_key(&self) -> [u8; 32];
    fn salt(&self) -> [u8; 16];
}

pub struct SystemEntropy;

impl Entropy for SystemEntropy {
    fn ephemeral_private_key(&self) -> [u8; 32] {
        loop {
            let mut candidate = [0u8; 32];
            if getrandom::getrandom(&mut candidate).is_ok()
                && SecretKey::from_slice(&candidate).is_ok()
            {
                return candidate;
            }
        }
    }

    fn salt(&self) -> [u8; 16] {
        let mut salt = [0u8; 16];
        let _ = getrandom::getrandom(&mut salt);
        salt
    }
}

/// `Buffer.from(text, 'base64url')`: either alphabet, padding optional, foreign characters skipped.
pub fn decode_base64_any(text: &str) -> Vec<u8> {
    base64_decode_lenient(&text.replace('-', "+").replace('_', "/"))
}

fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    hmac_sha256(salt, ikm)
}

fn hkdf_expand(prk: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(length);
    let mut previous: Vec<u8> = Vec::new();
    let mut counter = 0u8;
    while output.len() < length {
        counter += 1;
        let mut block = previous.clone();
        block.extend_from_slice(info);
        block.push(counter);
        previous = hmac_sha256(prk, &block).to_vec();
        output.extend_from_slice(&previous);
    }
    output.truncate(length);
    output
}

/// `generateNonce(base, counter)` of `http_ece`: the counter XORed into the last six bytes.
fn record_nonce(base: &[u8], counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&base[..12]);
    let tail = u64::from_be_bytes([
        0, 0, nonce[6], nonce[7], nonce[8], nonce[9], nonce[10], nonce[11],
    ]) ^ counter;
    nonce[6..12].copy_from_slice(&tail.to_be_bytes()[2..8]);
    nonce
}

/// `encryptionHelper.encrypt(p256dh, auth, payload, 'aes128gcm').cipherText`.
pub fn encrypt_aes128gcm(
    user_public_key: &str,
    user_auth: &str,
    payload: &[u8],
    entropy: &dyn Entropy,
) -> Result<Vec<u8>, EncryptError> {
    let user_public = decode_base64_any(user_public_key);
    if user_public.len() != 65 {
        return Err(fail(
            "The subscription p256dh value should be 65 bytes long.",
        ));
    }
    let auth_secret = decode_base64_any(user_auth);
    if auth_secret.len() < 16 {
        return Err(fail(
            "The subscription auth key should be at least 16 bytes long",
        ));
    }

    let secret_key = SecretKey::from_slice(&entropy.ephemeral_private_key())
        .map_err(|_| fail("Private key is not valid for specified curve."))?;
    let sender_public = secret_key.public_key().to_encoded_point(false);
    let receiver = PublicKey::from_sec1_bytes(&user_public)
        .map_err(|_| fail("Public key is not valid for specified curve"))?;
    let shared = diffie_hellman(secret_key.to_nonzero_scalar(), receiver.as_affine());

    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&user_public);
    key_info.extend_from_slice(sender_public.as_bytes());
    let secret = hkdf_expand(
        &hkdf_extract(&auth_secret, shared.raw_secret_bytes().as_slice()),
        &key_info,
        32,
    );

    let salt = entropy.salt();
    let prk = hkdf_extract(&salt, &secret);
    let key = hkdf_expand(&prk, b"Content-Encoding: aes128gcm\0", 16);
    let nonce_base = hkdf_expand(&prk, b"Content-Encoding: nonce\0", 12);
    let cipher = Aes128Gcm::new(GenericArray::from_slice(&key));

    let mut out = Vec::with_capacity(payload.len() + 100);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&(RECORD_SIZE as u32).to_be_bytes());
    out.push(sender_public.as_bytes().len() as u8);
    out.extend_from_slice(sender_public.as_bytes());

    let data_per_record = RECORD_SIZE - 1 - TAG_LENGTH;
    let mut start = 0usize;
    let mut counter = 0u64;
    loop {
        let end = start + data_per_record;
        let last = end >= payload.len();
        let mut plain = payload[start.min(payload.len())..end.min(payload.len())].to_vec();
        plain.push(if last { 2 } else { 1 });
        let nonce = record_nonce(&nonce_base, counter);
        let block = cipher
            .encrypt(GenericArray::from_slice(&nonce), plain.as_slice())
            .map_err(|_| fail("invalid tag generated"))?;
        out.extend_from_slice(&block);
        if last {
            return Ok(out);
        }
        start = end;
        counter += 1;
    }
}

fn validate_subject(subject: &str) -> Result<(), EncryptError> {
    if subject.is_empty() {
        return Err(fail("No subject set in vapidDetails.subject."));
    }
    let parsed = url::Url::parse(subject)
        .map_err(|_| EncryptError(format!("Vapid subject is not a valid URL. {subject}")))?;
    if !matches!(parsed.scheme(), "https" | "mailto") {
        return Err(EncryptError(format!(
            "Vapid subject is not an https: or mailto: URL. {subject}"
        )));
    }
    Ok(())
}

fn is_url_safe_base64(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn validate_key(key: &str, missing: &str, label: &str, length: usize) -> Result<(), EncryptError> {
    if key.is_empty() {
        return Err(fail(missing));
    }
    if !is_url_safe_base64(key) {
        return Err(EncryptError(format!(
            "Vapid {label} key must be a URL safe Base 64 (without \"=\")"
        )));
    }
    if decode_base64_any(key).len() != length {
        return Err(EncryptError(format!(
            "Vapid {label} key should be {length} bytes long when decoded."
        )));
    }
    Ok(())
}

/// The compact ES256 JWT of `getVapidHeaders` (after the audience, subject and key checks of the SDK).
pub fn vapid_jwt(
    audience: &str,
    subject: &str,
    public_key: &str,
    private_key: &str,
    expiration: i64,
) -> Result<String, EncryptError> {
    if url::Url::parse(audience).is_err() {
        return Err(EncryptError(format!(
            "VAPID audience is not a url. {audience}"
        )));
    }
    validate_subject(subject)?;
    validate_key(
        public_key,
        "No key set vapidDetails.publicKey",
        "public",
        65,
    )?;
    validate_key(
        private_key,
        "No key set in vapidDetails.privateKey",
        "private",
        32,
    )?;

    let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = format!(
        "{{\"aud\":{},\"exp\":{expiration},\"sub\":{}}}",
        serde_json::Value::String(audience.to_string()),
        serde_json::Value::String(subject.to_string())
    );
    let signing_input = format!("{header}.{}", URL_SAFE_NO_PAD.encode(claims.as_bytes()));
    let signing_key = SigningKey::from_slice(&decode_base64_any(private_key))
        .map_err(|_| fail("Vapid private key is not a valid P-256 scalar."))?;
    let signature: Signature = signing_key.sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed;
    impl Entropy for Fixed {
        fn ephemeral_private_key(&self) -> [u8; 32] {
            let mut k = [0u8; 32];
            k[31] = 7;
            k
        }
        fn salt(&self) -> [u8; 16] {
            [3u8; 16]
        }
    }

    #[test]
    fn hkdf_matches_rfc_5869_test_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0u8..=0x0c).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        let okm = hkdf_expand(&hkdf_extract(&salt, &ikm), &info, 42);
        let hex: String = okm.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn the_record_nonce_xors_the_counter_into_the_last_six_bytes() {
        let base: Vec<u8> = (1u8..=12).collect();
        assert_eq!(record_nonce(&base, 0).to_vec(), base);
        let n = record_nonce(&base, 1);
        assert_eq!(n[11], base[11] ^ 1);
        assert_eq!(&n[..11], &base[..11]);
    }

    #[test]
    fn short_keys_are_refused_with_the_sdk_messages() {
        let ok = URL_SAFE_NO_PAD.encode([4u8; 65]);
        let auth = URL_SAFE_NO_PAD.encode([1u8; 16]);
        assert_eq!(
            encrypt_aes128gcm("AAAA", &auth, b"x", &Fixed),
            Err(fail(
                "The subscription p256dh value should be 65 bytes long."
            ))
        );
        assert_eq!(
            encrypt_aes128gcm(&ok, "AAAA", b"x", &Fixed),
            Err(fail(
                "The subscription auth key should be at least 16 bytes long"
            ))
        );
        assert_eq!(
            encrypt_aes128gcm(&ok, &auth, b"x", &Fixed),
            Err(fail("Public key is not valid for specified curve"))
        );
    }

    #[test]
    fn vapid_checks_run_in_the_sdk_order() {
        let public = URL_SAFE_NO_PAD.encode([4u8; 65]);
        let private = URL_SAFE_NO_PAD.encode([9u8; 32]);
        let a = "https://push.example.test";
        assert_eq!(
            vapid_jwt(a, "", &public, &private, 1),
            Err(fail("No subject set in vapidDetails.subject."))
        );
        assert_eq!(
            vapid_jwt(a, "mailto:a@b.c", "", &private, 1),
            Err(fail("No key set vapidDetails.publicKey"))
        );
        assert_eq!(
            vapid_jwt(a, "mailto:a@b.c", &public, "", 1),
            Err(fail("No key set in vapidDetails.privateKey"))
        );
        assert!(vapid_jwt(a, "mailto:a@b.c", &public, &private, 1).is_ok());
    }
}
