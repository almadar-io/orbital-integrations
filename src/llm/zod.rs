//! The reply checks the TS provider expresses as Zod schemas, with Zod's issue messages
//! (`path: message` joined by `; `, as `parseJsonResponse` formats them).

use serde_json::{Map, Value};

#[derive(Debug)]
pub struct Issue {
    pub path: Vec<String>,
    pub message: String,
}

impl Issue {
    fn at(path: &[String], key: &str, message: impl Into<String>) -> Self {
        let mut full = path.to_vec();
        if !key.is_empty() {
            full.push(key.to_string());
        }
        Self {
            path: full,
            message: message.into(),
        }
    }
}

pub fn format_issues(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(|i| format!("{}: {}", i.path.join("."), i.message))
        .collect::<Vec<_>>()
        .join("; ")
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

fn expect_object<'a>(
    value: &'a Value,
    path: &[String],
) -> Result<&'a Map<String, Value>, Vec<Issue>> {
    value.as_object().ok_or_else(|| {
        vec![Issue::at(
            path,
            "",
            format!("Expected object, received {}", received(value)),
        )]
    })
}

fn string_at(
    map: &Map<String, Value>,
    key: &str,
    path: &[String],
    issues: &mut Vec<Issue>,
) -> Option<String> {
    match map.get(key) {
        None => {
            issues.push(Issue::at(path, key, "Required"));
            None
        }
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => {
            issues.push(Issue::at(
                path,
                key,
                format!("Expected string, received {}", received(other)),
            ));
            None
        }
    }
}

/// `z.number().min(0).max(1)`.
fn unit_number_at(
    map: &Map<String, Value>,
    key: &str,
    path: &[String],
    issues: &mut Vec<Issue>,
) -> Option<Value> {
    match map.get(key) {
        None => {
            issues.push(Issue::at(path, key, "Required"));
            None
        }
        Some(Value::Number(n)) => {
            let x = n.as_f64().unwrap_or(f64::NAN);
            if x < 0.0 {
                issues.push(Issue::at(
                    path,
                    key,
                    "Number must be greater than or equal to 0",
                ));
                None
            } else if x > 1.0 {
                issues.push(Issue::at(
                    path,
                    key,
                    "Number must be less than or equal to 1",
                ));
                None
            } else {
                Some(Value::Number(n.clone()))
            }
        }
        Some(other) => {
            issues.push(Issue::at(
                path,
                key,
                format!("Expected number, received {}", received(other)),
            ));
            None
        }
    }
}

/// `{ category: enum(categories), confidence: number 0..1, reasoning: string }`, unknown keys stripped.
pub fn classification(value: &Value, categories: &[String]) -> Result<Value, Vec<Issue>> {
    let map = expect_object(value, &[])?;
    let mut issues = Vec::new();
    let options = categories
        .iter()
        .map(|c| format!("'{c}'"))
        .collect::<Vec<_>>()
        .join(" | ");
    let category = match map.get("category") {
        None => {
            issues.push(Issue::at(&[], "category", "Required"));
            None
        }
        Some(Value::String(s)) if categories.contains(s) => Some(s.clone()),
        Some(Value::String(s)) => {
            issues.push(Issue::at(
                &[],
                "category",
                format!("Invalid enum value. Expected {options}, received '{s}'"),
            ));
            None
        }
        Some(other) => {
            issues.push(Issue::at(
                &[],
                "category",
                format!("Expected {options}, received {}", received(other)),
            ));
            None
        }
    };
    let confidence = unit_number_at(map, "confidence", &[], &mut issues);
    let reasoning = string_at(map, "reasoning", &[], &mut issues);
    match (category, confidence, reasoning) {
        (Some(c), Some(conf), Some(r)) if issues.is_empty() => {
            let mut out = Map::new();
            out.insert("category".into(), Value::String(c));
            out.insert("confidence".into(), conf);
            out.insert("reasoning".into(), Value::String(r));
            Ok(Value::Object(out))
        }
        _ => Err(issues),
    }
}

/// `{ summary: string, keyPoints: string[] }`, unknown keys stripped.
pub fn summary(value: &Value) -> Result<Value, Vec<Issue>> {
    let map = expect_object(value, &[])?;
    let mut issues = Vec::new();
    let summary = string_at(map, "summary", &[], &mut issues);
    let mut points = Vec::new();
    match map.get("keyPoints") {
        None => issues.push(Issue::at(&[], "keyPoints", "Required")),
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::String(s) => points.push(Value::String(s.clone())),
                    other => issues.push(Issue::at(
                        &["keyPoints".to_string()],
                        &i.to_string(),
                        format!("Expected string, received {}", received(other)),
                    )),
                }
            }
        }
        Some(other) => issues.push(Issue::at(
            &[],
            "keyPoints",
            format!("Expected array, received {}", received(other)),
        )),
    }
    match summary {
        Some(s) if issues.is_empty() => {
            let mut out = Map::new();
            out.insert("summary".into(), Value::String(s));
            out.insert("keyPoints".into(), Value::Array(points));
            Ok(Value::Object(out))
        }
        _ => Err(issues),
    }
}

/// `{ verdicts: [{ id: string, category: string, confidence: number 0..1 }] }` whose categories are all declared.
pub fn verdicts(
    value: &Value,
    categories: &[String],
) -> Result<Vec<(String, String, Value)>, Vec<Issue>> {
    let map = expect_object(value, &[])?;
    let mut issues = Vec::new();
    let mut out = Vec::new();
    match map.get("verdicts") {
        None => issues.push(Issue::at(&[], "verdicts", "Required")),
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let path = vec!["verdicts".to_string(), i.to_string()];
                let Some(entry) = item.as_object() else {
                    issues.push(Issue::at(
                        &path,
                        "",
                        format!("Expected object, received {}", received(item)),
                    ));
                    continue;
                };
                let id = string_at(entry, "id", &path, &mut issues);
                let category = string_at(entry, "category", &path, &mut issues);
                let confidence = unit_number_at(entry, "confidence", &path, &mut issues);
                if let (Some(id), Some(category), Some(confidence)) = (id, category, confidence) {
                    out.push((id, category, confidence));
                }
            }
        }
        Some(other) => issues.push(Issue::at(
            &[],
            "verdicts",
            format!("Expected array, received {}", received(other)),
        )),
    }
    if !issues.is_empty() {
        return Err(issues);
    }
    if !out
        .iter()
        .all(|(_, category, _)| categories.contains(category))
    {
        return Err(vec![Issue {
            path: Vec::new(),
            message: "every category must be a declared one".to_string(),
        }]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cats() -> Vec<String> {
        vec!["billing".to_string(), "support".to_string()]
    }

    fn message(issues: Result<Value, Vec<Issue>>) -> String {
        format_issues(&issues.unwrap_err())
    }

    #[test]
    fn classification_keeps_the_declared_keys_only() {
        let out = classification(
            &json!({"category":"billing","confidence":0.5,"reasoning":"r","extra":1}),
            &cats(),
        )
        .unwrap();
        assert_eq!(
            out,
            json!({"category":"billing","confidence":0.5,"reasoning":"r"})
        );
    }

    #[test]
    fn classification_rejects_an_undeclared_category_with_zods_message() {
        let bad = classification(
            &json!({"category":"sales","confidence":0.5,"reasoning":"r"}),
            &cats(),
        );
        assert_eq!(
            message(bad),
            "category: Invalid enum value. Expected 'billing' | 'support', received 'sales'"
        );
    }

    #[test]
    fn classification_reports_every_failing_field_in_shape_order() {
        let bad = classification(&json!({"confidence":1.5}), &cats());
        assert_eq!(message(bad), "category: Required; confidence: Number must be less than or equal to 1; reasoning: Required");
    }

    #[test]
    fn classification_rejects_a_non_object_and_a_negative_confidence() {
        assert_eq!(
            message(classification(&json!([1]), &cats())),
            ": Expected object, received array"
        );
        assert_eq!(
            message(classification(
                &json!({"category":"billing","confidence":-0.1,"reasoning":"r"}),
                &cats()
            )),
            "confidence: Number must be greater than or equal to 0"
        );
    }

    #[test]
    fn summary_checks_each_key_point() {
        assert_eq!(
            summary(&json!({"summary":"s","keyPoints":["a"]})).unwrap(),
            json!({"summary":"s","keyPoints":["a"]})
        );
        assert_eq!(
            message(summary(&json!({"summary":"s","keyPoints":["a", 2]}))),
            "keyPoints.1: Expected string, received number"
        );
        assert_eq!(
            message(summary(&json!({"summary":"s","keyPoints":"a"}))),
            "keyPoints: Expected array, received string"
        );
    }

    #[test]
    fn verdicts_require_declared_categories() {
        let ok = verdicts(
            &json!({"verdicts":[{"id":"1","category":"billing","confidence":0.9}]}),
            &cats(),
        )
        .unwrap();
        assert_eq!(ok.len(), 1);
        let bad = verdicts(
            &json!({"verdicts":[{"id":"1","category":"sales","confidence":0.9}]}),
            &cats(),
        );
        assert_eq!(
            format_issues(&bad.unwrap_err()),
            ": every category must be a declared one"
        );
    }
}
