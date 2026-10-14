//! `String.prototype.localeCompare` as the in-memory object listing sorts with it, for the ASCII range: ICU's
//! root collation puts punctuation and symbols first, then digits, then letters, case-insensitively at the first
//! level, with the lowercase form first when only the case differs.

use std::cmp::Ordering;

/// ICU primary order of printable ASCII (`Intl.Collator`, base sensitivity); letters share a group per case pair.
const ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789aAbBcCdDeEfFgGhHiIjJkKlLmMnNoOpPqQrRsStTuUvVwWxXyYzZ";

fn primary(c: char) -> usize {
    if c.is_ascii_uppercase() {
        return primary(c.to_ascii_lowercase());
    }
    ORDER
        .chars()
        .position(|o| o == c || o.to_ascii_lowercase() == c)
        .unwrap_or(ORDER.len() + c as usize)
}

pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let (a_chars, b_chars): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    for (x, y) in a_chars.iter().zip(&b_chars) {
        let by_primary = primary(*x).cmp(&primary(*y));
        if by_primary != Ordering::Equal {
            return by_primary;
        }
    }
    let by_length = a_chars.len().cmp(&b_chars.len());
    if by_length != Ordering::Equal {
        return by_length;
    }
    for (x, y) in a_chars.iter().zip(&b_chars) {
        if x != y {
            return if x.is_ascii_uppercase() {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punctuation_sorts_before_digits_before_letters() {
        assert_eq!(locale_compare("a-b", "a.b"), Ordering::Less);
        assert_eq!(locale_compare("a/b", "a0"), Ordering::Less);
        assert_eq!(locale_compare("a9", "aa"), Ordering::Less);
        assert_eq!(locale_compare("a_b", "a-b"), Ordering::Less);
    }

    #[test]
    fn case_only_differences_put_lowercase_first_but_letters_dominate_case() {
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("Docs/Three", "docs/one"), Ordering::Greater);
        assert_eq!(locale_compare("B", "a"), Ordering::Greater);
        assert_eq!(locale_compare("ab", "aB"), Ordering::Less);
        assert_eq!(locale_compare("x", "x"), Ordering::Equal);
    }

    #[test]
    fn a_prefix_sorts_first() {
        assert_eq!(locale_compare("docs", "docs/"), Ordering::Less);
        assert_eq!(locale_compare("docs-extra/x", "docs/x"), Ordering::Less);
    }

    #[test]
    fn matches_what_node_printed_for_random_pairs() {
        let pairs: Vec<(String, String, i32)> =
            serde_json::from_str(include_str!("collate_pairs.json")).expect("pairs");
        assert!(pairs.len() >= 50);
        for (a, b, expected) in pairs {
            assert_eq!(locale_compare(&a, &b) as i32, expected, "{a:?} vs {b:?}");
        }
    }
}
