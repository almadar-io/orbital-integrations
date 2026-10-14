//! Conversions between the kernel's `orbital_core::Value` and `serde_json::Value`.

use orbital_core::Value;
use serde_json::{Map, Number, Value as Json};

/// A JSON number that keeps whole values integral, so `64` stays `64` on the wire.
pub fn number(n: f64) -> Json {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Json::Number(Number::from(n as i64))
    } else {
        Number::from_f64(n).map_or(Json::Null, Json::Number)
    }
}

pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Number(n) => number(*n),
        Value::String(s) => Json::String(s.clone()),
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), to_json(v)))
                .collect::<Map<String, Json>>(),
        ),
        other => serde_json::to_value(other).unwrap_or(Json::Null),
    }
}

pub fn from_json(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        Json::String(s) => Value::String(s.clone()),
        Json::Array(items) => Value::Array(items.iter().map(from_json).collect()),
        Json::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), from_json(v))).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whole_numbers_stay_integral_and_fractions_stay_fractional() {
        assert_eq!(number(64.0), json!(64));
        assert_eq!(number(0.25), json!(0.25));
        assert_eq!(number(f64::NAN), Json::Null);
    }

    #[test]
    fn a_value_survives_the_round_trip() {
        let original = json!({ "a": [1, 2.5, "x", null, true], "b": { "c": 3 } });
        assert_eq!(to_json(&from_json(&original)), original);
    }
}
