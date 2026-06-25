use super::super::{
    parse_github_owner_repo, set_configured_gitlab_hosts_for_remote_detection_for_test,
};

#[test]
fn parse_github_owner_repo_table_accepts_provider_neutral_scopes() {
    set_configured_gitlab_hosts_for_remote_detection_for_test(Vec::new());
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
        ("git@gitlab.com:foo/bar.git", Some("gitlab.com/foo/bar")),
        (
            "https://gitlab.com/group/sub/project.git",
            Some("gitlab.com/group/sub/project"),
        ),
        ("git@gitlab.corp.example:platform/api.git", None),
        ("https://bitbucket.org/tanstack/router.git", None),
        ("https://github.com/tanstack/router/extra", None),
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

    set_configured_gitlab_hosts_for_remote_detection_for_test(vec![
        "gitlab.corp.example:8443".to_owned(),
    ]);
    assert_eq!(
        parse_github_owner_repo("ssh://git@gitlab.corp.example:8443/platform/api.git"),
        Some("gitlab.corp.example:8443/platform/api".to_owned())
    );
    assert_eq!(
        parse_github_owner_repo("https://gitlab.attacker.example/platform/api.git"),
        None
    );
    set_configured_gitlab_hosts_for_remote_detection_for_test(Vec::new());
}
