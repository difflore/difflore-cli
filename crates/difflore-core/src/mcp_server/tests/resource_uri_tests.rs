//! Parse tests for the citation resource URIs, kept separate from the
//! integration tests so the URI surface can be tested without a real
//! `SqlitePool`.

use super::super::{parse_signature_uri, parse_verdict_uri};

#[test]
fn parse_verdict_uri_table() {
    // Subpath rejection prevents collisions with future resource shapes;
    // empty / non-verdict / non-signature IDs must all return None so the
    // router falls through to the generic 404.
    let cases: &[(&str, Option<&str>)] = &[
        ("difflore://verdicts/ext-abc123", Some("ext-abc123")),
        ("difflore://verdicts/01J8F-9Z3QH", Some("01J8F-9Z3QH")),
        ("difflore://verdicts/abc/child", None),
        ("difflore://verdicts/", None),
        ("difflore://verdicts/   ", None),
        ("difflore://rules/active", None),
        ("difflore://signatures/abc", None),
    ];
    for (uri, expected) in cases {
        assert_eq!(
            parse_verdict_uri(uri),
            expected.map(String::from),
            "uri: {uri}"
        );
    }
}

#[test]
fn parse_signature_uri_table() {
    assert_eq!(
        parse_signature_uri("difflore://signatures/deadbeef"),
        Some("deadbeef".to_owned())
    );
    assert_eq!(parse_signature_uri("difflore://signatures/"), None);
}
