//! JS number-to-text, for the providers that interpolate a number into a URL or a CSV cell.

/// JS `Number.prototype.toString` (radix 10) for a finite number.
pub fn js_number(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    if x == 0.0 {
        return "0".to_string();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let sci = format!("{:e}", x.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let sign_e = if e < 0 { '-' } else { '+' };
        if k == 1 {
            format!("{digits}e{sign_e}{}", e.abs())
        } else {
            format!("{}.{}e{sign_e}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

/// JS `Number(text)` for a string: surrounding whitespace is ignored, empty is 0, a malformed literal is NaN.
pub fn js_number_from_text(text: &str) -> f64 {
    let t = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    let (sign, unsigned) = match t.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, t.strip_prefix('+').unwrap_or(t)),
    };
    if unsigned == "Infinity" {
        return sign * f64::INFINITY;
    }
    let decimal = unsigned.starts_with(|c: char| c.is_ascii_digit() || c == '.')
        && unsigned
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && unsigned.bytes().any(|b| b.is_ascii_digit());
    if !decimal {
        return f64::NAN;
    }
    unsigned.parse::<f64>().map_or(f64::NAN, |n| sign * n)
}

/// JS `String(value)` for a JSON value (`null` is empty only inside an array join, as in `[1,null].join()`).
pub fn js_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => js_number(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                if item.is_null() {
                    String::new()
                } else {
                    js_string(item)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        serde_json::Value::Object(_) => "[object Object]".to_string(),
    }
}

/// JS truthiness of a JSON value.
pub fn js_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        serde_json::Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// JS `Number(value)` for a JSON scalar; arrays and objects are not converted here and read as NaN.
pub fn js_to_number(value: &serde_json::Value) -> f64 {
    match value {
        serde_json::Value::Null => 0.0,
        serde_json::Value::Bool(b) => f64::from(u8::from(*b)),
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        serde_json::Value::String(s) => js_number_from_text(s),
        _ => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_the_way_js_prints_them() {
        for (x, want) in [
            (100.0, "100"),
            (10.5, "10.5"),
            (1.995, "1.995"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (123456789012345680000.0, "123456789012345680000"),
            (-0.25, "-0.25"),
            (1.5e-10, "1.5e-10"),
            (0.0, "0"),
        ] {
            assert_eq!(js_number(x), want, "{x}");
        }
    }

    #[test]
    fn text_converts_like_js_number() {
        for (text, want) in [
            ("12.50", 12.5),
            (" 12 ", 12.0),
            ("", 0.0),
            ("-3", -3.0),
            ("1e3", 1000.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("0x10", 16.0),
        ] {
            assert_eq!(js_number_from_text(text), want, "{text:?}");
        }
        for text in [
            "n/a", "1,5", "--1", "e5", "inf", "nan", "1.2.3", "12px", "0x", "+-1",
        ] {
            assert!(js_number_from_text(text).is_nan(), "{text:?}");
        }
        assert_eq!(js_number_from_text("-Infinity"), f64::NEG_INFINITY);
    }
}
