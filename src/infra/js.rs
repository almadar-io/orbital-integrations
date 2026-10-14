//! JavaScript number formatting and date parsing the in-memory providers' results depend on.

use crate::wallclock::{iso8601, parse_iso8601};

/// `Number.prototype.toString(radix)` for a finite number: V8's `DoubleToRadixCString`, so a fractional value
/// prints the shortest digits that round-trip, exactly as `Math.random().toString(36)` does.
pub fn number_to_radix_string(value: f64, radix: u32) -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0.0 {
        return "0".into();
    }
    let negative = value < 0.0;
    let value = value.abs();
    let radix_f = f64::from(radix);
    let mut integer = value.floor();
    let mut fraction = value - integer;
    let next = f64::from_bits(value.to_bits() + 1);
    let mut delta = (0.5 * (next - value)).max(f64::from_bits(1));
    let mut digits: Vec<u8> = Vec::new();
    if fraction >= delta {
        loop {
            fraction *= radix_f;
            delta *= radix_f;
            let digit = fraction as usize;
            digits.push(CHARS[digit]);
            fraction -= digit as f64;
            if (fraction > 0.5 || (fraction == 0.5 && digit & 1 == 1)) && fraction + delta > 1.0 {
                loop {
                    match digits.pop() {
                        None => {
                            integer += 1.0;
                            break;
                        }
                        Some(c) => {
                            let d = if c > b'9' { c - b'a' + 10 } else { c - b'0' } as u32;
                            if d + 1 < radix {
                                digits.push(CHARS[(d + 1) as usize]);
                                break;
                            }
                        }
                    }
                }
                break;
            }
            if fraction < delta {
                break;
            }
        }
    }
    let mut int_digits: Vec<u8> = Vec::new();
    while integer >= 9_007_199_254_740_992.0 {
        integer /= radix_f;
        int_digits.push(b'0');
    }
    loop {
        let remainder = integer % radix_f;
        int_digits.push(CHARS[remainder as usize]);
        integer = (integer - remainder) / radix_f;
        if integer <= 0.0 {
            break;
        }
    }
    int_digits.reverse();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&String::from_utf8_lossy(&int_digits));
    if !digits.is_empty() {
        out.push('.');
        out.push_str(&String::from_utf8_lossy(&digits));
    }
    out
}

/// `new Date(text).getTime()` for the ISO-8601 forms (`YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS[.fff]][Z|±HH:MM]`); a
/// form without an offset is read as UTC. `None` stands for `NaN`, which includes every non-ISO spelling.
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let slice = b.get(from..from + len)?;
        if slice.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(slice).ok()?.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut ms = parse_iso8601(&format!("{year:04}-{month:02}-{day:02}T00:00:00.000Z"))?;
    if iso8601(ms)[..10] != text[..10] {
        return None;
    }
    if b.len() == 10 {
        return Some(ms);
    }
    if b.get(10) != Some(&b'T') || b.get(13) != Some(&b':') {
        return None;
    }
    let (hour, minute) = (num(11, 2)?, num(14, 2)?);
    if hour > 24 || minute > 59 {
        return None;
    }
    ms += hour * 3_600_000 + minute * 60_000;
    let mut at = 16;
    if b.get(at) == Some(&b':') {
        let second = num(at + 1, 2)?;
        if second > 59 {
            return None;
        }
        ms += second * 1000;
        at += 3;
        if b.get(at) == Some(&b'.') {
            let start = at + 1;
            let mut end = start;
            while b.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                return None;
            }
            let frac = &text[start..end];
            let padded = format!("{frac:0<3}");
            ms += padded[..3].parse::<i64>().ok()?;
            at = end;
        }
    }
    match &text[at..] {
        "" | "Z" => Some(ms),
        zone if zone.len() == 6 && (zone.starts_with('+') || zone.starts_with('-')) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (zh, zm) = (num(at + 1, 2)?, num(at + 4, 2)?);
            if b.get(at + 3) != Some(&b':') {
                return None;
            }
            Some(ms - sign * (zh * 3_600_000 + zm * 60_000))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radix_matches_what_v8_prints() {
        assert_eq!(number_to_radix_string(0.5, 36), "0.i");
        assert_eq!(number_to_radix_string(0.123456789012, 36), "0.4fzzzxkwghv");
        assert_eq!(number_to_radix_string(0.987654321098, 36), "0.zk00008napk");
        assert_eq!(number_to_radix_string(0.0000123456, 36), "0.000kqhomh1n3cj");
        assert_eq!(number_to_radix_string(0.1 + 0.2, 36), "0.asssssssssv");
        assert_eq!(number_to_radix_string(1e-7, 36), "0.000061oezo085tl");
        assert_eq!(number_to_radix_string(255.0, 16), "ff");
        assert_eq!(number_to_radix_string(0.0, 36), "0");
        assert_eq!(number_to_radix_string(35.0, 36), "z");
        assert_eq!(number_to_radix_string(36.0, 36), "10");
    }

    #[test]
    fn iso_parses_the_epoch_and_a_recent_instant() {
        assert_eq!(
            parse_iso_ms("2025-10-09T08:53:20.123Z"),
            Some(1_760_000_000_123)
        );
        assert_eq!(parse_iso_ms("2025-10-09"), Some(1_759_968_000_000));
        assert_eq!(parse_iso_ms("1969-12-31T23:59:59.999Z"), Some(-1));
    }

    #[test]
    fn offsets_and_fraction_widths_follow_ecmascript() {
        assert_eq!(
            parse_iso_ms("2025-10-09T10:53:20+02:00"),
            Some(1_760_000_000_000)
        );
        assert_eq!(
            parse_iso_ms("2025-10-09T08:53:20.1Z"),
            Some(1_760_000_000_100)
        );
        assert_eq!(parse_iso_ms("2025-10-09T08:53Z"), Some(1_759_999_980_000));
        assert_eq!(parse_iso_ms("not a date"), None);
        assert_eq!(parse_iso_ms("2025-13-01"), None);
        assert_eq!(parse_iso_ms("2025-02-30"), None);
        assert_eq!(parse_iso_ms("2025-10-09T08:53:20.Z"), None);
    }
}
