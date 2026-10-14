//! Read-only SQL enforcement: the twin of `assertReadOnlySelect` (`integrations/database/sql-guard.ts`). It
//! accepts exactly one SELECT statement (optionally CTE-led). Comments are stripped and string and identifier
//! contents blanked before the keyword check, so denied words inside literals never false-positive and comment
//! or quote tricks cannot smuggle a second statement past it.

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardResult {
    pub ok: bool,
    pub reason: Option<String>,
}

const DENIED_KEYWORDS: [&str; 6] = ["INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "FOR"];

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// JS `String.prototype.trim`: Unicode white space and line terminators, and the BOM, but not U+0085.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

fn skip_single_quoted(sql: &[char], start: usize, e_string: bool) -> usize {
    let mut i = start + 1;
    while i < sql.len() {
        if e_string && sql[i] == '\\' {
            i += 2;
            continue;
        }
        if sql[i] == '\'' {
            if sql.get(i + 1) == Some(&'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    sql.len()
}

fn skip_double_quoted(sql: &[char], start: usize) -> usize {
    let mut i = start + 1;
    while i < sql.len() {
        if sql[i] == '"' {
            if sql.get(i + 1) == Some(&'"') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    sql.len()
}

/// A dollar-quote tag (`$$` or `$tag$`) at `start`; `$1` placeholders are not tags.
fn match_dollar_tag(sql: &[char], start: usize) -> Option<Vec<char>> {
    let mut i = start + 1;
    if sql.get(i) == Some(&'$') {
        return Some(vec!['$', '$']);
    }
    let first = *sql.get(i)?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    i += 1;
    while i < sql.len() && is_word_char(sql[i]) && sql[i] != '$' {
        i += 1;
    }
    if sql.get(i) != Some(&'$') {
        return None;
    }
    Some(sql[start..=i].to_vec())
}

fn find_tag(sql: &[char], tag: &[char], from: usize) -> Option<usize> {
    (from..sql.len().saturating_sub(tag.len().saturating_sub(1)))
        .find(|&at| sql[at..].starts_with(tag))
}

fn sanitize(source: &str) -> String {
    let sql: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < sql.len() {
        let ch = sql[i];
        if ch == '-' && sql.get(i + 1) == Some(&'-') {
            while i < sql.len() && sql[i] != '\n' {
                i += 1;
            }
            out.push(' ');
            continue;
        }
        if ch == '/' && sql.get(i + 1) == Some(&'*') {
            let mut depth = 1;
            i += 2;
            while i < sql.len() && depth > 0 {
                if sql[i] == '/' && sql.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if sql[i] == '*' && sql.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }
        if ch == '\'' {
            let mut j = i as isize - 1;
            while j >= 0 && matches!(sql[j as usize], ' ' | '\t' | '\n' | '\r') {
                j -= 1;
            }
            let e_string = j >= 0
                && matches!(sql[j as usize], 'e' | 'E')
                && (j == 0 || !is_word_char(sql[j as usize - 1]));
            i = skip_single_quoted(&sql, i, e_string);
            out.push_str("''");
            continue;
        }
        if ch == '"' {
            i = skip_double_quoted(&sql, i);
            out.push_str("\"\"");
            continue;
        }
        if ch == '$' {
            if let Some(tag) = match_dollar_tag(&sql, i) {
                i = match find_tag(&sql, &tag, i + tag.len()) {
                    Some(close) => close + tag.len(),
                    None => sql.len(),
                };
                out.push_str("$$");
                continue;
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Verify `sql` is a single read-only SELECT statement.
pub fn assert_read_only_select(sql: &str) -> GuardResult {
    let fail = |reason: String| GuardResult {
        ok: false,
        reason: Some(reason),
    };
    let sanitized = sanitize(sql);
    let statements: Vec<&str> = sanitized
        .split(';')
        .map(js_trim)
        .filter(|s| !s.is_empty())
        .collect();
    match statements.len() {
        0 => return fail("Empty statement".into()),
        1 => {}
        _ => return fail("Only a single statement is allowed".into()),
    }
    let word = Regex::new(r"(?-u:\b)[A-Za-z_][A-Za-z0-9_]*(?-u:\b)").expect("word pattern");
    let words: Vec<String> = word
        .find_iter(statements[0])
        .map(|m| m.as_str().to_ascii_uppercase())
        .collect();
    match words.first().map(String::as_str) {
        Some("SELECT") | Some("WITH") => {}
        _ => return fail("Only SELECT statements are allowed".into()),
    }
    if let Some(denied) = words.iter().find(|w| DENIED_KEYWORDS.contains(&w.as_str())) {
        return fail(format!(
            "Keyword not allowed in a read-only query: {denied}"
        ));
    }
    GuardResult {
        ok: true,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_select_and_rejects_a_second_statement() {
        assert!(assert_read_only_select("SELECT 1;").ok);
        assert_eq!(
            assert_read_only_select("SELECT 1; SELECT 2")
                .reason
                .as_deref(),
            Some("Only a single statement is allowed")
        );
    }

    #[test]
    fn a_denied_word_inside_a_literal_is_inert_but_outside_it_is_not() {
        assert!(assert_read_only_select("SELECT 'delete into'").ok);
        assert!(!assert_read_only_select("SELECT x FROM t FOR UPDATE").ok);
    }

    #[test]
    fn the_byte_order_mark_trims_but_the_next_line_character_does_not() {
        assert_eq!(
            assert_read_only_select("\u{feff}").reason.as_deref(),
            Some("Empty statement")
        );
        assert_eq!(
            assert_read_only_select("\u{85}").reason.as_deref(),
            Some("Only SELECT statements are allowed")
        );
    }

    #[test]
    fn word_boundaries_are_ascii_like_javascript() {
        assert!(assert_read_only_select("\u{e9}SELECT 1").ok);
    }
}
