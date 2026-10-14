//! HMAC-SHA256 (RFC 2104) over the `sha2` digest, the `createHmac('sha256', key).update(data).digest('hex')` of Node.

use sha2::{Digest, Sha256};

const BLOCK: usize = 64;

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

pub fn hmac_sha256_hex(key: &str, data: &str) -> String {
    hmac_sha256(key.as_bytes(), data.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_4231_test_case_2() {
        assert_eq!(
            hmac_sha256_hex("Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn rfc_4231_test_case_6_a_key_longer_than_the_block_is_hashed_first() {
        let long_key = vec![0xaau8; 131];
        let mac = hmac_sha256(
            &long_key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn control_a_different_key_gives_a_different_mac() {
        assert_ne!(hmac_sha256_hex("a", "m"), hmac_sha256_hex("b", "m"));
    }
}
