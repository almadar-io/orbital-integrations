//! stripe-node's wire encodings: `qs` form bodies with bracket keys, `encodeURIComponent` path segments,
//! `stripe-node-retry-<uuid4>` idempotency keys and `new Date(seconds * 1000).toISOString()`.

use serde_json::Value;

use crate::error::IntegrationError;
pub use crate::formwire::{encode_component, encode_path_segment, number_text, Pairs};
use crate::jswire::iso_from_ms;

/// `qs.stringify` of one entry: nested objects and arrays become bracketed keys, `null` an empty value,
/// `undefined` (an absent param) nothing.
pub fn flatten(pairs: &mut Pairs, key: &str, value: Option<&Value>) {
    match value {
        None => {}
        Some(Value::Null) => pairs.push_empty(key),
        Some(Value::Bool(b)) => pairs.push(key, if *b { "true" } else { "false" }),
        Some(Value::Number(n)) => pairs.push(key, &number_text(n.as_f64().unwrap_or(f64::NAN))),
        Some(Value::String(s)) => pairs.push(key, s),
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                flatten(pairs, &format!("{key}[{i}]"), Some(item));
            }
        }
        Some(Value::Object(map)) => {
            for (k, v) in map {
                flatten(pairs, &format!("{key}[{}]", encode_component(k)), Some(v));
            }
        }
    }
}

/// `stripe-node-retry-<uuid v4>`.
pub fn idempotency_key() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        bytes = [0x5a; 16];
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "stripe-node-retry-{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `new Date(seconds * 1000).toISOString()`; a missing or out-of-range value is a `RangeError`, as in JS.
pub fn iso_from_unix(seconds: Option<&Value>) -> Result<String, IntegrationError> {
    let invalid = || IntegrationError::plain("Invalid time value");
    let seconds = match seconds {
        Some(Value::Number(n)) => n.as_f64().ok_or_else(invalid)?,
        _ => return Err(invalid()),
    };
    let ms = (seconds * 1000.0).trunc();
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return Err(invalid());
    }
    Ok(iso_from_ms(ms as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn components_encode_like_qs() {
        assert_eq!(encode_component("o 1&2"), "o%201%262");
        assert_eq!(
            encode_component("caf\u{e9} (rush)"),
            "caf%C3%A9%20%28rush%29"
        );
        assert_eq!(encode_component("a-b.c_d~e"), "a-b.c_d~e");
    }

    #[test]
    fn path_segments_encode_like_encode_uri_component() {
        assert_eq!(encode_path_segment("sub 1/x?y"), "sub%201%2Fx%3Fy");
        assert_eq!(encode_path_segment("a!b*c'd(e)"), "a!b*c'd(e)");
    }

    #[test]
    fn nested_values_flatten_to_bracketed_keys() {
        let mut pairs = Pairs::default();
        flatten(
            &mut pairs,
            "metadata",
            Some(&json!({ "a b": "1", "n": 2, "t": true, "z": null, "o": {}, "l": ["x"] })),
        );
        flatten(&mut pairs, "missing", None);
        let body = pairs.encode();
        for part in [
            "metadata[a%20b]=1",
            "metadata[n]=2",
            "metadata[t]=true",
            "metadata[z]=",
            "metadata[l][0]=x",
        ] {
            assert!(body.contains(part), "{body} lacks {part}");
        }
        assert!(!body.contains("missing") && !body.contains("[o]"));
    }

    #[test]
    fn numbers_print_like_js() {
        assert_eq!(number_text(2000.0), "2000");
        assert_eq!(number_text(12.5), "12.5");
        assert_eq!(number_text(-3.0), "-3");
    }

    #[test]
    fn unix_seconds_become_iso_timestamps() {
        assert_eq!(
            iso_from_unix(Some(&json!(1767225600))).unwrap(),
            "2026-01-01T00:00:00.000Z"
        );
        assert_eq!(
            iso_from_unix(Some(&json!(0))).unwrap(),
            "1970-01-01T00:00:00.000Z"
        );
        assert_eq!(
            iso_from_unix(Some(&json!(1709210096.789))).unwrap(),
            "2024-02-29T12:34:56.789Z"
        );
        assert_eq!(
            iso_from_unix(Some(&json!(-1))).unwrap(),
            "1969-12-31T23:59:59.000Z"
        );
        assert!(iso_from_unix(None).is_err());
        assert!(iso_from_unix(Some(&json!("x"))).is_err());
    }

    #[test]
    fn idempotency_keys_are_uuid_v4_and_unique() {
        let a = idempotency_key();
        let b = idempotency_key();
        assert_ne!(a, b);
        let id = a.strip_prefix("stripe-node-retry-").unwrap();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
    }
}
