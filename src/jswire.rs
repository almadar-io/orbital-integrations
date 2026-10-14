//! JavaScript-compatible text and error behavior the `fetch`-based TS providers (banking, esign) share.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Number, Value};

use crate::error::IntegrationError;
use crate::transport::TransportError;

/// `s.slice(0, max)`: `max` counts UTF-16 code units, as in JS.
pub fn js_slice(s: &str, max: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(max).collect();
    String::from_utf16_lossy(&units)
}

pub fn base64_encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// `Buffer.from(text, 'base64')`: lenient about padding and whitespace, skips characters outside the alphabet.
pub fn base64_decode_lenient(text: &str) -> Vec<u8> {
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
        .collect();
    let trimmed = cleaned.trim_end_matches('=');
    let mut out = Vec::with_capacity(trimmed.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in trimmed.chars() {
        let v = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            _ => 63,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// What Node's `fetch` rejects with when no response arrives (`AbortSignal.timeout` or a refused connection).
pub fn fetch_failure(error: TransportError) -> IntegrationError {
    match error {
        TransportError::TimedOut => {
            IntegrationError::plain("The operation was aborted due to timeout")
        }
        TransportError::Failed(_) => IntegrationError::plain("fetch failed"),
    }
}

/// The URL `fetch` puts on the wire: parsed and re-serialized per the WHATWG URL standard (spaces and other
/// path characters percent-encoded, fragment dropped). An unparsable string passes through, as `fetch` would reject it.
pub fn fetch_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut parsed) => {
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => raw.to_string(),
    }
}

/// `new Date(ms).toISOString()` for a time inside the JS `Date` range.
pub fn iso_from_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s, milli) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    let year_text = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    };
    format!("{year_text}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{milli:03}Z")
}

/// `Date.parse` for the ISO 8601 forms the calendar params use: `YYYY-MM-DD` (UTC) and
/// `YYYY-MM-DDTHH:MM[:SS[.fff]][Z|+HH:MM|-HH:MM]`. A time without an offset is read as UTC (JS reads it as
/// host-local time). Anything else is `None` (`NaN` in JS).
pub fn parse_date_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let digits = |from: usize, len: usize| -> Option<i64> {
        let part = text.get(from..from + len)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let (year, month, day) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?);
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let mut ms = days_from_civil(year, month, day) * 86_400_000;
    let rest = &text[10..];
    if rest.is_empty() {
        return Some(ms);
    }
    let time = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let tb = time.as_bytes();
    if tb.len() < 5 || tb[2] != b':' {
        return None;
    }
    let tdigits = |from: usize, len: usize| -> Option<i64> {
        let part = time.get(from..from + len)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let (hour, minute) = (tdigits(0, 2)?, tdigits(3, 2)?);
    let mut cursor = 5;
    let mut second = 0;
    let mut milli = 0;
    if tb.get(cursor) == Some(&b':') {
        second = tdigits(cursor + 1, 2)?;
        cursor += 3;
        if tb.get(cursor) == Some(&b'.') {
            let frac: String = time[cursor + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if frac.is_empty() {
                return None;
            }
            milli = format!("{frac:0<3}")[..3].parse().ok()?;
            cursor += 1 + frac.len();
        }
    }
    if hour > 24
        || minute > 59
        || second > 59
        || (hour == 24 && (minute > 0 || second > 0 || milli > 0))
    {
        return None;
    }
    ms += ((hour * 60 + minute) * 60 + second) * 1000 + milli;
    let zone = &time[cursor..];
    let offset_minutes = match zone {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match zone.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let body = &zone[1..];
            let (oh, om) = match body.len() {
                5 if body.as_bytes()[2] == b':' => (
                    body[..2].parse::<i64>().ok()?,
                    body[3..].parse::<i64>().ok()?,
                ),
                4 => (
                    body[..2].parse::<i64>().ok()?,
                    body[2..].parse::<i64>().ok()?,
                ),
                _ => return None,
            };
            sign * (oh * 60 + om)
        }
    };
    Some(ms - offset_minutes * 60_000)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Milliseconds since the epoch: `Date.now()`. Injectable so token expiry is testable.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    })
}

/// `Number(value)` for a JSON value: numbers pass, strings parse like JS (blank is 0, junk is NaN),
/// `null` is 0, anything else is NaN.
pub fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Null) => 0.0,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::String(s)) => js_string_number(s),
        Some(_) => f64::NAN,
    }
}

fn js_string_number(text: &str) -> f64 {
    let t = text.trim();
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let numeric = t
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'));
    if !numeric {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// A parsed JSON value that remembers the order its object keys were written in.
enum Ordered {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Ordered>),
    Object(Vec<(String, Ordered)>),
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Ordered, E> {
                Ok(Ordered::Bool(v))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Ordered, E> {
                Ok(Ordered::Number(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Ordered, E> {
                Ok(Ordered::Number(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Ordered, E> {
                Number::from_f64(v)
                    .map(Ordered::Number)
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Ordered, E> {
                Ok(Ordered::String(v.to_string()))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }
            fn visit_none<E: de::Error>(self) -> Result<Ordered, E> {
                Ok(Ordered::Null)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ordered, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Ordered::Array(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut entries: Vec<(String, Ordered)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Ordered>()? {
                    match entries.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => entries.push((key, value)),
                    }
                }
                Ok(Ordered::Object(entries))
            }
        }
        deserializer.deserialize_any(V)
    }
}

fn write_ordered(value: &Ordered, out: &mut String) {
    match value {
        Ordered::Null => out.push_str("null"),
        Ordered::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Ordered::Number(n) => out.push_str(&n.to_string()),
        Ordered::String(s) => out.push_str(&Value::String(s.clone()).to_string()),
        Ordered::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_ordered(item, out);
            }
            out.push(']');
        }
        Ordered::Object(entries) => {
            out.push('{');
            for (i, (key, item)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_ordered(item, out);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(JSON.parse(text))`: the compact form with object keys in the order they were written
/// (serde_json's `Value` sorts them).
pub fn json_restringify(text: &str) -> Result<String, String> {
    let parsed: Ordered = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut out = String::new();
    write_ordered(&parsed, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_counts_utf16_units() {
        assert_eq!(js_slice("abcdef", 3), "abc");
        assert_eq!(js_slice("ab", 5), "ab");
        assert_eq!(js_slice("a\u{1F600}b", 3), "a\u{1F600}");
    }

    #[test]
    fn base64_round_trips_and_tolerates_padding_and_whitespace() {
        assert_eq!(base64_encode(b"hello contract"), "aGVsbG8gY29udHJhY3Q=");
        assert_eq!(
            base64_decode_lenient("aGVsbG8gY29udHJhY3Q="),
            b"hello contract"
        );
        assert_eq!(base64_decode_lenient("aGVs bG8"), b"hello");
        assert_eq!(base64_decode_lenient(""), b"");
    }

    #[test]
    fn dates_round_trip_through_iso_text() {
        for (text, ms) in [
            ("2026-01-01", 1_767_225_600_000),
            ("2026-01-01T00:00:00Z", 1_767_225_600_000),
            ("2026-01-01T01:00:00+01:00", 1_767_225_600_000),
            ("2026-01-01T00:00:00.5Z", 1_767_225_600_500),
            ("2026-01-01T09:30", 1_767_225_600_000 + 34_200_000),
            ("1969-12-31T23:59:59Z", -1000),
        ] {
            assert_eq!(parse_date_ms(text), Some(ms), "{text}");
        }
        assert_eq!(iso_from_ms(1_767_225_600_000), "2026-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(-1000), "1969-12-31T23:59:59.000Z");
        for bad in [
            "",
            "2026-13-01",
            "2026-02-30",
            "not a date",
            "2026-01-01T25:00:00Z",
            "2026-01-01T10:00:00+9",
        ] {
            assert_eq!(parse_date_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn fetch_url_encodes_the_path_like_whatwg() {
        assert_eq!(
            fetch_url("https://h.test/a/acc 1/x/?q=1 2"),
            "https://h.test/a/acc%201/x/?q=1%202"
        );
        assert_eq!(fetch_url("https://h.test/a#frag"), "https://h.test/a");
        assert_eq!(fetch_url("not a url"), "not a url");
    }

    #[test]
    fn restringify_keeps_key_order_and_compacts() {
        let text = r#" { "z": 1, "a": [true, null, {"y": "s", "b": 2.5}], "m": "q\"x" } "#;
        assert_eq!(
            json_restringify(text).unwrap(),
            r#"{"z":1,"a":[true,null,{"y":"s","b":2.5}],"m":"q\"x"}"#
        );
        assert!(json_restringify("{nope").is_err());
        assert_eq!(json_restringify(r#"{"a":1,"a":2}"#).unwrap(), r#"{"a":2}"#);
    }

    #[test]
    fn js_number_follows_number_coercion() {
        let n = |v: &Value| js_number(Some(v));
        assert_eq!(n(&Value::String("-45.50".into())), -45.5);
        assert_eq!(n(&Value::String("  ".into())), 0.0);
        assert_eq!(n(&Value::String("0x10".into())), 16.0);
        assert!(n(&Value::String("12abc".into())).is_nan());
        assert!(n(&Value::String("inf".into())).is_nan());
        assert_eq!(n(&Value::String("Infinity".into())), f64::INFINITY);
        assert_eq!(js_number(None), 0.0);
        assert_eq!(n(&Value::Null), 0.0);
    }

    #[test]
    fn a_dead_connection_reads_like_node_fetch() {
        assert_eq!(
            fetch_failure(TransportError::Failed("refused".into())).message,
            "fetch failed"
        );
        assert!(fetch_failure(TransportError::TimedOut)
            .message
            .contains("timeout"));
    }
}
