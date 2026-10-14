//! Narrowing helpers for property values, the twin of `engine/coerce.ts`. `None` stands for a JS `undefined`.

use serde_json::Value;

pub use crate::jsnumber::js_number;

/// JS `String(v ?? '')`.
pub fn str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(other) => js_string(other),
    }
}

pub fn num(v: Option<&Value>) -> f64 {
    opt_num(v).unwrap_or(0.0)
}

pub fn bool(v: Option<&Value>) -> bool {
    opt_bool(v).unwrap_or(false)
}

pub fn str_arr(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn opt_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

pub fn opt_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

pub fn opt_bool(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

/// JS `String(value)` for a JSON value.
pub fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn str_matches_js_string_coercion() {
        assert_eq!(str(Some(&json!("a"))), "a");
        assert_eq!(str(None), "");
        assert_eq!(str(Some(&Value::Null)), "");
        assert_eq!(str(Some(&json!(3))), "3");
        assert_eq!(str(Some(&json!(true))), "true");
        assert_eq!(str(Some(&json!([1, null, "x"]))), "1,,x");
        assert_eq!(str(Some(&json!({"a": 1}))), "[object Object]");
    }

    #[test]
    fn numeric_and_boolean_narrowing_default_when_wrong_typed() {
        assert_eq!(num(Some(&json!(2.5))), 2.5);
        assert_eq!(num(Some(&json!("2"))), 0.0);
        assert!(!bool(Some(&json!("true"))));
        assert_eq!(opt_num(Some(&json!("2"))), None);
        assert_eq!(opt_bool(Some(&json!(false))), Some(false));
    }

    #[test]
    fn str_arr_drops_non_strings() {
        assert_eq!(str_arr(Some(&json!(["a", 1, "b"]))), vec!["a", "b"]);
        assert!(str_arr(Some(&json!("a"))).is_empty());
    }
}
