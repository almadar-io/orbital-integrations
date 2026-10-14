//! The two percent-encoders `fetch` callers lean on: `encodeURIComponent` and `URLSearchParams`.

fn push_escaped(out: &mut String, byte: u8) {
    out.push('%');
    out.push(char::from_digit(u32::from(byte >> 4), 16).map_or('0', |c| c.to_ascii_uppercase()));
    out.push(char::from_digit(u32::from(byte & 15), 16).map_or('0', |c| c.to_ascii_uppercase()));
}

/// JS `encodeURIComponent`: everything but `A-Za-z0-9 - _ . ! ~ * ' ( )` is escaped.
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            push_escaped(&mut out, byte);
        }
    }
    out
}

/// `URLSearchParams` serialization of one value (`application/x-www-form-urlencoded`): a space is `+`,
/// everything but `A-Za-z0-9 * - . _` is escaped.
pub fn form_urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"*-._".contains(&byte) {
            out.push(byte as char);
        } else if byte == b' ' {
            out.push('+');
        } else {
            push_escaped(&mut out, byte);
        }
    }
    out
}

/// `qs.stringify` (RFC 3986) of one value: everything but `A-Za-z0-9 - . _ ~` is escaped, so `!'()*` are too.
pub fn encode_rfc3986(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            push_escaped(&mut out, byte);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_keeps_the_js_unreserved_set() {
        assert_eq!(
            encode_uri_component("a b&c=d/é!~*'()-_."),
            "a%20b%26c%3Dd%2F%C3%A9!~*'()-_."
        );
    }

    #[test]
    fn form_encoding_turns_spaces_into_plus_and_escapes_the_rest() {
        assert_eq!(form_urlencode("a b!~'()é"), "a+b%21%7E%27%28%29%C3%A9");
        assert_eq!(form_urlencode("{\"a\":1}"), "%7B%22a%22%3A1%7D");
    }

    #[test]
    fn rfc3986_also_escapes_the_js_reserved_marks() {
        assert_eq!(
            encode_rfc3986("a!b'c(d)e*f~g_h-i.j k"),
            "a%21b%27c%28d%29e%2Af~g_h-i.j%20k"
        );
    }
}
