use super::api::{decode_base64_lenient, js_number, parse_repo_url};

#[test]
fn repo_urls_yield_owner_and_repo_without_the_git_suffix() {
    assert_eq!(
        parse_repo_url("https://github.com/acme/widgets.git").expect("parses"),
        ("acme".to_string(), "widgets".to_string())
    );
    assert_eq!(
        parse_repo_url("https://github.com/acme/widgets/tree/main").expect("parses"),
        ("acme".to_string(), "widgets".to_string())
    );
}

#[test]
fn control_a_url_with_one_segment_or_none_is_rejected() {
    for bad in [
        "https://github.com/acme",
        "https://github.com/",
        "no scheme",
    ] {
        let error = parse_repo_url(bad).expect_err(bad);
        assert_eq!(error.code.as_deref(), Some("VALIDATION_ERROR"));
        assert_eq!(
            error.message,
            format!("Failed to parse repository URL: {bad}")
        );
    }
}

#[test]
fn base64_decoding_is_as_lenient_as_node() {
    assert_eq!(decode_base64_lenient("aGVsbG8="), "hello");
    assert_eq!(decode_base64_lenient("aGVs\nbG8="), "hello");
    assert_eq!(decode_base64_lenient("aGVsbG8"), "hello");
    assert_eq!(decode_base64_lenient("aGVsbG8=trailing ignored"), "hello");
    assert_eq!(decode_base64_lenient(""), "");
    assert_eq!(decode_base64_lenient("_-8"), "\u{fffd}\u{fffd}");
}

#[test]
fn numbers_print_like_js_template_strings() {
    assert_eq!(js_number(&serde_json::json!(7)), "7");
    assert_eq!(js_number(&serde_json::json!(7.0)), "7");
    assert_eq!(js_number(&serde_json::json!(2.5)), "2.5");
}
