use super::super::parse_github_owner_repo;

#[test]
fn parse_github_owner_repo_table() {
    let cases: &[(&str, Option<&str>)] = &[
        (
            "git@github.com:difflore-fixtures/router.git",
            Some("difflore-fixtures/router"),
        ),
        (
            "https://github.com/tanstack/router.git",
            Some("tanstack/router"),
        ),
        (
            "https://github.com/tanstack/router",
            Some("tanstack/router"),
        ),
        ("git@gitlab.com:foo/bar.git", None),
        ("", None),
        ("https://github.com/", None),
    ];
    for (input, expected) in cases {
        assert_eq!(
            parse_github_owner_repo(input),
            expected.map(String::from),
            "input: {input}"
        );
    }
}
