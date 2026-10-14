//! Reading JSON out of a model reply: the twin of `@almadar/llm` `json-parser.ts`.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static pattern"))
}

fn extract_balanced(text: &str, start: usize, open: char, close: char) -> Option<String> {
    if !text[start..].starts_with(open) {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    for (offset, ch) in text[start..].char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape_next = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(text[start..start + offset + ch.len_utf8()].to_string());
            }
        }
    }
    None
}

fn greedy_span(text: &str, start: usize, close: char) -> Option<String> {
    let end = text.rfind(close)?;
    (end > start).then(|| text[start..=end].to_string())
}

pub fn extract_json_from_text(text: &str) -> Option<String> {
    static THINK: OnceLock<Regex> = OnceLock::new();
    static FENCE: OnceLock<Regex> = OnceLock::new();
    static NUMBER: OnceLock<Regex> = OnceLock::new();
    let stripped = re(&THINK, r"(?s)<think>.*?</think>\s*").replace_all(text, "");
    let trimmed = stripped.trim();

    if let Some(caps) = re(&FENCE, r"(?s)```(?:json)?\s*(.*?)```").captures(trimmed) {
        return Some(caps[1].trim().to_string());
    }

    let object_start = trimmed.find('{');
    let array_start = trimmed.find('[');
    let object_first =
        object_start.is_some() && (array_start.is_none() || object_start < array_start);
    let array_first =
        array_start.is_some() && (object_start.is_none() || array_start < object_start);

    if array_first {
        let start = array_start?;
        if let Some(found) = extract_balanced(trimmed, start, '[', ']') {
            return Some(found);
        }
        if let Some(found) = greedy_span(trimmed, start, ']') {
            return Some(found);
        }
    }
    if object_first {
        let start = object_start?;
        if let Some(found) = extract_balanced(trimmed, start, '{', '}') {
            return Some(found);
        }
        if let Some(found) = greedy_span(trimmed, start, '}') {
            return Some(found);
        }
    }

    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return Some(trimmed.to_string());
    }
    if re(&NUMBER, r"^-?\d+(\.\d+)?([eE][+-]?\d+)?$").is_match(trimmed)
        || matches!(trimmed, "true" | "false" | "null")
    {
        return Some(trimmed.to_string());
    }
    None
}

fn fix_common_json_issues(json: &str) -> String {
    static TRAILING: OnceLock<Regex> = OnceLock::new();
    static BARE_KEY: OnceLock<Regex> = OnceLock::new();
    static CONTROL: OnceLock<Regex> = OnceLock::new();
    let fixed = re(&TRAILING, r",(\s*[}\]])")
        .replace_all(json, "$1")
        .into_owned();
    let fixed = re(&BARE_KEY, r"([{,]\s*)([A-Za-z0-9_]+)(\s*:)")
        .replace_all(&fixed, "$1\"$2\"$3")
        .into_owned();
    let fixed = fixed.replace('\'', "\"");
    re(&CONTROL, r"[\x00-\x1F\x7F]")
        .replace_all(&fixed, " ")
        .into_owned()
}

/// The first 200 characters, as JS `substring(0, 200)` counts UTF-16 units for the ASCII a model replies in.
fn head_200(text: &str) -> String {
    text.chars().take(200).collect()
}

/// The reply's JSON value, or the error message `parseJsonResponse` throws.
pub fn parse_json_response(response: &str) -> Result<Value, String> {
    let Some(json) = extract_json_from_text(response) else {
        return Err("No valid JSON found in response. Expected a JSON value (object, array, string, number, boolean, or null), possibly wrapped in markdown code blocks.".to_string());
    };
    match serde_json::from_str::<Value>(&json) {
        Ok(value) => Ok(value),
        Err(first) => serde_json::from_str::<Value>(&fix_common_json_issues(&json)).map_err(|_| {
            format!(
                "Failed to parse JSON: {first}. Raw text: {}...",
                head_200(&json)
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_bare_object() {
        assert_eq!(parse_json_response(r#"{"a":1}"#), Ok(json!({"a": 1})));
    }

    #[test]
    fn reads_a_fenced_block_with_or_without_the_json_tag() {
        assert_eq!(
            parse_json_response("```json\n{\"a\":1}\n```"),
            Ok(json!({"a": 1}))
        );
        assert_eq!(parse_json_response("```\n[1,2]\n```"), Ok(json!([1, 2])));
    }

    #[test]
    fn reads_an_object_surrounded_by_prose() {
        assert_eq!(
            parse_json_response(r#"Sure! {"a":"}"} hope that helps"#),
            Ok(json!({"a": "}"}))
        );
    }

    #[test]
    fn strips_thinking_tags_first() {
        assert_eq!(
            parse_json_response("<think>{\"no\":1}</think>\n{\"yes\":2}"),
            Ok(json!({"yes": 2}))
        );
    }

    #[test]
    fn the_earlier_bracket_wins() {
        assert_eq!(parse_json_response(r#"[{"a":1}]"#), Ok(json!([{"a": 1}])));
        assert_eq!(parse_json_response(r#"{"a":[1]}"#), Ok(json!({"a": [1]})));
    }

    #[test]
    fn reads_primitives() {
        assert_eq!(parse_json_response("42"), Ok(json!(42)));
        assert_eq!(parse_json_response("true"), Ok(json!(true)));
        assert_eq!(parse_json_response("null"), Ok(Value::Null));
        assert_eq!(parse_json_response("\"hi\""), Ok(json!("hi")));
    }

    #[test]
    fn repairs_trailing_commas_bare_keys_and_single_quotes() {
        assert_eq!(
            parse_json_response("{a: 1, 'b': 2,}"),
            Ok(json!({"a": 1, "b": 2}))
        );
    }

    #[test]
    fn plain_prose_is_the_no_json_error_the_retry_prompt_quotes() {
        assert_eq!(
            parse_json_response("not json at all").unwrap_err(),
            "No valid JSON found in response. Expected a JSON value (object, array, string, number, boolean, or null), possibly wrapped in markdown code blocks."
        );
    }

    #[test]
    fn an_unrepairable_object_reports_the_raw_text() {
        let err = parse_json_response("{\"a\": ").unwrap_err();
        assert!(
            err.starts_with("Failed to parse JSON: ") || err.starts_with("No valid JSON"),
            "{err}"
        );
    }
}
