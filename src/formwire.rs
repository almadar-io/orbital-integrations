//! Form and URL encodings the stripe-node and googleapis clients share: `qs` bodies and queries with
//! bracket keys, `encodeURIComponent` path segments, JS number text.

/// `(key, value)` pairs in the order stripe-node writes them. A key may carry literal brackets
/// (`line_items[0][price]`); its components are already percent-encoded.
#[derive(Default, Clone)]
pub struct Pairs(Vec<(String, String)>);

impl Pairs {
    pub fn push(&mut self, key: &str, value: &str) {
        self.0.push((key.to_string(), encode_component(value)));
    }

    /// An entry with an empty value (`qs` writes `null` as `key=`).
    pub fn push_empty(&mut self, key: &str) {
        self.0.push((key.to_string(), String::new()));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn encode(&self) -> String {
        self.0
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }
}

/// `qs`'s RFC 3986 encoding: unreserved characters stay, everything else is `%XX` of its UTF-8 bytes.
pub fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `encodeURIComponent`: keeps `A-Za-z0-9 - _ . ! ~ * ' ( )`.
pub fn encode_path_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A JS number as text: whole values without a fraction.
pub fn number_text(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}
