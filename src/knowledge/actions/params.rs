//! Param checks with zod's wording: the twin of each TS action's `params` schema. An action lists its fields
//! and gets back the one `VALIDATION_ERROR` message the TS integration reports (`path: message`, issues joined by `; `).

use serde_json::{Map, Value};

use crate::knowledge::store::{KnowledgeError, StoreResult};

#[derive(Clone)]
pub enum Ty {
    /// `z.string()`; `min1` adds `.min(1)`, `trim` applies `.trim()` first.
    Str {
        min1: bool,
        trim: bool,
    },
    Num,
    /// `z.number().int()` with optional bounds.
    Int {
        min: Option<f64>,
        max: Option<f64>,
    },
    Bool,
    Enum(&'static [&'static str]),
    Array {
        item: Box<Ty>,
        min1: bool,
    },
    Object(Vec<Field>),
    /// Any JSON object.
    Record,
}

#[derive(Clone)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    pub optional: bool,
}

/// A required string of at least one character.
pub fn id(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Str {
            min1: true,
            trim: false,
        },
        optional: false,
    }
}

pub fn opt_id(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Str {
            min1: true,
            trim: false,
        },
        optional: true,
    }
}

/// A required string with no length rule.
pub fn text(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Str {
            min1: false,
            trim: false,
        },
        optional: false,
    }
}

pub fn opt_text(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Str {
            min1: false,
            trim: false,
        },
        optional: true,
    }
}

/// A required string that must have a character after `.trim()`.
pub fn trimmed(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Str {
            min1: true,
            trim: true,
        },
        optional: false,
    }
}

pub fn opt_trimmed(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Str {
            min1: true,
            trim: true,
        },
        optional: true,
    }
}

pub fn flag(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Bool,
        optional: false,
    }
}

pub fn opt_flag(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Bool,
        optional: true,
    }
}

pub fn int(name: &'static str) -> Field {
    Field {
        name,
        ty: Ty::Int {
            min: None,
            max: None,
        },
        optional: false,
    }
}

pub fn opt_int(name: &'static str, min: Option<f64>, max: Option<f64>) -> Field {
    Field {
        name,
        ty: Ty::Int { min, max },
        optional: true,
    }
}

pub fn pick(name: &'static str, values: &'static [&'static str]) -> Field {
    Field {
        name,
        ty: Ty::Enum(values),
        optional: false,
    }
}

pub fn opt_pick(name: &'static str, values: &'static [&'static str]) -> Field {
    Field {
        name,
        ty: Ty::Enum(values),
        optional: true,
    }
}

pub fn list(name: &'static str, item: Ty, min1: bool) -> Field {
    Field {
        name,
        ty: Ty::Array {
            item: Box::new(item),
            min1,
        },
        optional: false,
    }
}

pub fn opt_list(name: &'static str, item: Ty, min1: bool) -> Field {
    Field {
        name,
        ty: Ty::Array {
            item: Box::new(item),
            min1,
        },
        optional: true,
    }
}

pub fn object(name: &'static str, fields: Vec<Field>, optional: bool) -> Field {
    Field {
        name,
        ty: Ty::Object(fields),
        optional,
    }
}

pub fn id_ty() -> Ty {
    Ty::Str {
        min1: true,
        trim: false,
    }
}

fn received(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_integral(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|f| f.is_finite() && f.fract() == 0.0)
}

fn check(value: &Value, ty: &Ty, path: &mut Vec<String>, issues: &mut Vec<String>) {
    let mut at =
        |message: String, path: &Vec<String>| issues.push(format!("{}: {message}", path.join(".")));
    match ty {
        Ty::Str { min1, trim } => match value.as_str() {
            None => at(
                format!("Expected string, received {}", received(value)),
                path,
            ),
            Some(s) => {
                let shown = if *trim { s.trim() } else { s };
                if *min1 && shown.is_empty() {
                    at(
                        "String must contain at least 1 character(s)".to_string(),
                        path,
                    );
                }
            }
        },
        Ty::Num => {
            if !value.is_number() {
                at(
                    format!("Expected number, received {}", received(value)),
                    path,
                );
            }
        }
        Ty::Int { min, max } => {
            if !value.is_number() {
                at(
                    format!("Expected number, received {}", received(value)),
                    path,
                );
            } else if !is_integral(value) {
                at("Expected integer, received float".to_string(), path);
            } else {
                let n = value.as_f64().unwrap_or(0.0);
                if let Some(lo) = min.filter(|lo| n < *lo) {
                    at(
                        format!(
                            "Number must be greater than or equal to {}",
                            crate::jsnumber::js_number(lo)
                        ),
                        path,
                    );
                }
                if let Some(hi) = max.filter(|hi| n > *hi) {
                    at(
                        format!(
                            "Number must be less than or equal to {}",
                            crate::jsnumber::js_number(hi)
                        ),
                        path,
                    );
                }
            }
        }
        Ty::Bool => {
            if !value.is_boolean() {
                at(
                    format!("Expected boolean, received {}", received(value)),
                    path,
                );
            }
        }
        Ty::Enum(values) => {
            let joined = values
                .iter()
                .map(|v| format!("'{v}'"))
                .collect::<Vec<_>>()
                .join(" | ");
            match value.as_str() {
                Some(s) if values.contains(&s) => {}
                Some(s) => at(
                    format!("Invalid enum value. Expected {joined}, received '{s}'"),
                    path,
                ),
                None => at(
                    format!("Expected {}, received {}", joined, received(value)),
                    path,
                ),
            }
        }
        Ty::Array { item, min1 } => match value.as_array() {
            None => at(
                format!("Expected array, received {}", received(value)),
                path,
            ),
            Some(items) => {
                if *min1 && items.is_empty() {
                    at("Array must contain at least 1 element(s)".to_string(), path);
                }
                for (i, element) in items.iter().enumerate() {
                    path.push(i.to_string());
                    check(element, item, path, issues);
                    path.pop();
                }
            }
        },
        Ty::Object(fields) => match value.as_object() {
            None => at(
                format!("Expected object, received {}", received(value)),
                path,
            ),
            Some(map) => check_fields(map, fields, path, issues),
        },
        Ty::Record => {
            if !value.is_object() {
                at(
                    format!("Expected object, received {}", received(value)),
                    path,
                );
            }
        }
    }
}

fn check_fields(
    map: &Map<String, Value>,
    fields: &[Field],
    path: &mut Vec<String>,
    issues: &mut Vec<String>,
) {
    for field in fields {
        path.push(field.name.to_string());
        match map.get(field.name) {
            None => {
                if !field.optional {
                    issues.push(format!("{}: Required", path.join(".")));
                }
            }
            Some(value) => check(value, &field.ty, path, issues),
        }
        path.pop();
    }
}

/// Check `params` against `fields`; the error is the TS integration's `VALIDATION_ERROR`.
pub fn validate(params: &Map<String, Value>, fields: &[Field]) -> StoreResult<()> {
    let mut issues = Vec::new();
    check_fields(params, fields, &mut Vec::new(), &mut issues);
    if issues.is_empty() {
        Ok(())
    } else {
        Err(KnowledgeError::validation(issues.join("; ")))
    }
}

/// A refinement issue on the whole object, as `z.object().refine(...)` reports it (empty path).
pub fn refine_failed(message: &str) -> KnowledgeError {
    KnowledgeError::validation(format!(": {message}"))
}

pub fn str_of<'a>(params: &'a Map<String, Value>, key: &str) -> &'a str {
    params.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub fn opt_str_of<'a>(params: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str)
}

pub fn opt_f64_of(params: &Map<String, Value>, key: &str) -> Option<f64> {
    params.get(key).and_then(Value::as_f64)
}

pub fn opt_bool_of(params: &Map<String, Value>, key: &str) -> Option<bool> {
    params.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn a_blank_required_id_is_refused_with_zods_wording() {
        let err = validate(&map(json!({ "graphId": "" })), &[id("graphId")]).unwrap_err();
        assert_eq!(
            err.message,
            "graphId: String must contain at least 1 character(s)"
        );
    }

    #[test]
    fn missing_wrong_type_and_enum_issues_join_in_field_order() {
        let fields = [id("a"), flag("b"), pick("c", &["x", "y"])];
        let err = validate(&map(json!({ "b": "no", "c": "z" })), &fields).unwrap_err();
        assert_eq!(
            err.message,
            "a: Required; b: Expected boolean, received string; c: Invalid enum value. Expected 'x' | 'y', received 'z'"
        );
    }

    #[test]
    fn control_valid_params_pass_and_optional_fields_may_be_absent() {
        assert!(validate(&map(json!({ "a": "id" })), &[id("a"), opt_text("b")]).is_ok());
    }

    #[test]
    fn nested_array_items_are_pathed() {
        let item = Ty::Object(vec![id("ref")]);
        let err = validate(
            &map(json!({ "items": [{ "ref": "a" }, { "ref": "" }] })),
            &[list("items", item, true)],
        )
        .unwrap_err();
        assert_eq!(
            err.message,
            "items.1.ref: String must contain at least 1 character(s)"
        );
    }

    #[test]
    fn integers_reject_fractions_and_bounds() {
        let fields = [opt_int("limit", Some(1.0), Some(20.0))];
        assert_eq!(
            validate(&map(json!({ "limit": 1.5 })), &fields)
                .unwrap_err()
                .message,
            "limit: Expected integer, received float"
        );
        assert_eq!(
            validate(&map(json!({ "limit": 21 })), &fields)
                .unwrap_err()
                .message,
            "limit: Number must be less than or equal to 20"
        );
        assert!(validate(&map(json!({ "limit": 20 })), &fields).is_ok());
    }
}
