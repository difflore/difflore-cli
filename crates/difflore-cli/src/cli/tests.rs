use super::{AgentsCommands, Cli, CloudCommands, Commands, StatusLane, build_cli};
use clap::Parser;

#[test]
fn fix_help_states_pr_mode_is_local_only() {
    let mut command = build_cli();
    let fix = command
        .find_subcommand_mut("fix")
        .expect("fix command should exist");
    let help = fix.render_long_help().to_string();

    assert!(help.contains("GitHub PR"));
    assert!(help.contains("working tree"));
    assert!(help.contains("never commits"));
    assert!(help.contains("pushes"));
    assert!(help.contains("posts GitHub comments"));
}

#[test]
fn public_help_keeps_curated_command_surface() {
    let help = build_cli().render_long_help().to_string();

    for visible in [
        "  init",
        "  status",
        "  import-reviews",
        "  recall",
        "  fix",
        "  ask",
        "  cloud",
        "  agents",
        "  providers",
        "  embeddings",
        "  doctor",
    ] {
        assert!(help.contains(visible), "{visible} should be visible");
    }

    for removed in [
        "  plan",
        "  plan-pr",
        "  impact",
        "  upgrade",
        "  config",
        "  hook",
        "  review-inbox",
        "  reviewer",
        "  knowledge",
        "  ingest",
        "  candidates",
        "  explore",
        "  mcp-server",
        "  memory",
        "  mcp",
        "  lsp",
        "  migrate",
        "  demo",
        "  tui",
        "  dist",
        "  daemon",
        "  rules",
        "  sync",
        "  value-check",
    ] {
        assert!(!help.contains(removed), "{removed} should not be visible");
    }
}

#[test]
fn fix_help_keeps_launch_surface_short_but_pr_switches_parse() {
    let mut command = build_cli();
    let fix = command
        .find_subcommand_mut("fix")
        .expect("fix command should exist");
    let help = fix.render_long_help().to_string();
    let option_tokens = help
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect::<Vec<_>>();

    assert!(help.contains("--pr"));
    for hidden in [
        "--work-branch",
        "--no-checkout",
        "--allow-dirty",
        "--no-upload-acceptance",
        "--explain-rules",
        "--report",
        "--check",
        "--agent",
        "--repo",
        "--base",
    ] {
        assert!(
            !option_tokens.contains(&hidden),
            "{hidden} should stay hidden from fix help"
        );
    }
}

#[test]
fn fix_pr_safety_switches_parse_into_fix_args() {
    let cli = Cli::try_parse_from([
        "difflore",
        "fix",
        "--pr",
        "difflore/difflore-cli#42",
        "--work-branch",
        "local-pr-42",
        "--allow-dirty",
        "--no-upload-acceptance",
    ])
    .expect("fix PR safety switches should parse");

    match cli.command.expect("subcommand") {
        Commands::Fix(args) => {
            assert_eq!(args.pr.as_deref(), Some("difflore/difflore-cli#42"));
            assert_eq!(args.work_branch.as_deref(), Some("local-pr-42"));
            assert!(!args.no_checkout);
            assert!(args.allow_dirty);
            assert!(args.no_upload_acceptance);
        }
        _ => panic!("expected fix command"),
    }
}

#[test]
fn fix_pr_work_branch_conflicts_with_no_checkout_and_preview() {
    for args in [
        [
            "difflore",
            "fix",
            "--pr",
            "difflore/difflore-cli#42",
            "--work-branch",
            "local-pr-42",
            "--no-checkout",
        ]
        .as_slice(),
        [
            "difflore",
            "fix",
            "--pr",
            "difflore/difflore-cli#42",
            "--preview",
            "--work-branch",
            "local-pr-42",
        ]
        .as_slice(),
    ] {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "{args:?} should reject conflicting PR safety switches"
        );
    }
}

#[test]
fn fix_pr_safety_switches_require_pr_mode() {
    for args in [
        ["difflore", "fix", "--work-branch", "local-pr-42"].as_slice(),
        ["difflore", "fix", "--no-checkout"].as_slice(),
        ["difflore", "fix", "--allow-dirty"].as_slice(),
        ["difflore", "fix", "--no-upload-acceptance"].as_slice(),
    ] {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "{args:?} should require --pr"
        );
    }
}

#[test]
fn agents_command_parses_public_install_and_status_surface() {
    let install = Cli::try_parse_from(["difflore", "agents", "install", "--dry-run"])
        .expect("agents install should parse");
    assert!(matches!(
        install.command,
        Some(Commands::Agents {
            command: AgentsCommands::Install { dry_run: true }
        })
    ));

    let status = Cli::try_parse_from(["difflore", "agents", "status", "--json"])
        .expect("agents status should parse");
    assert!(matches!(
        status.command,
        Some(Commands::Agents {
            command: AgentsCommands::Status { json: true }
        })
    ));

    let uninstall = Cli::try_parse_from(["difflore", "agents", "uninstall", "--dry-run"])
        .expect("agents uninstall should parse");
    assert!(matches!(
        uninstall.command,
        Some(Commands::Agents {
            command: AgentsCommands::Uninstall { dry_run: true }
        })
    ));

    let uninstall_default = Cli::try_parse_from(["difflore", "agents", "uninstall"])
        .expect("agents uninstall without flag should parse");
    assert!(matches!(
        uninstall_default.command,
        Some(Commands::Agents {
            command: AgentsCommands::Uninstall { dry_run: false }
        })
    ));

    let update = Cli::try_parse_from(["difflore", "agents", "update", "--dry-run", "--force"])
        .expect("agents update should parse");
    assert!(matches!(
        update.command,
        Some(Commands::Agents {
            command: AgentsCommands::Update {
                dry_run: true,
                force: true
            }
        })
    ));

    let update_default = Cli::try_parse_from(["difflore", "agents", "update"])
        .expect("agents update without flags should parse");
    assert!(matches!(
        update_default.command,
        Some(Commands::Agents {
            command: AgentsCommands::Update {
                dry_run: false,
                force: false
            }
        })
    ));
}

#[test]
fn status_replaces_value_check_and_cloud_owns_sync() {
    let status =
        Cli::try_parse_from(["difflore", "status", "--json"]).expect("status should parse");
    assert!(matches!(
        status.command,
        Some(Commands::Status {
            json: true,
            lane: StatusLane::All,
        })
    ));

    let beta_status = Cli::try_parse_from(["difflore", "status", "--lane", "local-beta", "--json"])
        .expect("status local beta lane should parse");
    assert!(matches!(
        beta_status.command,
        Some(Commands::Status {
            json: true,
            lane: StatusLane::LocalBeta,
        })
    ));

    let ga_status = Cli::try_parse_from(["difflore", "status", "--lane", "production-ga"])
        .expect("status production GA lane should parse");
    assert!(matches!(
        ga_status.command,
        Some(Commands::Status {
            json: false,
            lane: StatusLane::ProductionGa,
        })
    ));

    assert!(
        Cli::try_parse_from(["difflore", "value-check"]).is_err(),
        "legacy value-check command should not parse"
    );

    let sync = Cli::try_parse_from(["difflore", "cloud", "sync", "--dry-run"])
        .expect("cloud sync should parse");
    assert!(matches!(
        sync.command,
        Some(Commands::Cloud {
            command: CloudCommands::Sync(super::SyncCliArgs { dry_run: true, .. })
        })
    ));

    let team =
        Cli::try_parse_from(["difflore", "cloud", "team", "--json"]).expect("team should parse");
    assert!(matches!(
        team.command,
        Some(Commands::Cloud {
            command: CloudCommands::Team { json: true }
        })
    ));

    let publish =
        Cli::try_parse_from(["difflore", "cloud", "publish", "--rule", "rule-1", "--json"])
            .expect("cloud publish should parse");
    assert!(matches!(
        publish.command,
        Some(Commands::Cloud {
            command: CloudCommands::Publish { rule, json: true, .. }
        }) if rule == "rule-1"
    ));

    let unpublish = Cli::try_parse_from([
        "difflore",
        "cloud",
        "unpublish",
        "--rule",
        "rule-1",
        "--json",
    ])
    .expect("cloud unpublish should parse");
    assert!(matches!(
        unpublish.command,
        Some(Commands::Cloud {
            command: CloudCommands::Unpublish { rule, json: true, .. }
        }) if rule == "rule-1"
    ));

    assert!(
        Cli::try_parse_from(["difflore", "sync"]).is_err(),
        "top-level sync should not parse"
    );
}

#[test]
fn removed_top_level_commands_do_not_parse() {
    for removed in [
        "plan",
        "plan-pr",
        "impact",
        "upgrade",
        "config",
        "hook",
        "review-inbox",
        "reviewer",
        "knowledge",
        "ingest",
        "candidates",
        "explore",
        "memory",
        "mcp",
        "lsp",
        "migrate",
        "demo",
        "tui",
        "dist",
        "daemon",
        "rules",
    ] {
        assert!(
            Cli::try_parse_from(["difflore", removed]).is_err(),
            "{removed} should not parse"
        );
    }
}

#[test]
fn hidden_mcp_server_transport_parses_but_stays_out_of_help() {
    let help = build_cli().render_long_help().to_string();
    assert!(!help.contains("  mcp-server"));
    assert!(!help.contains("  eval"));
    assert!(!help.contains("  trajectory"));
    assert!(!help.contains("  skills"));

    let parsed = Cli::try_parse_from(["difflore", "mcp-server"])
        .expect("hidden MCP transport should parse for installed agents");
    assert!(matches!(parsed.command, Some(Commands::McpServer)));

    assert!(Cli::try_parse_from(["difflore", "eval"]).is_ok());
    assert!(Cli::try_parse_from(["difflore", "trajectory", "review-id"]).is_ok());
    assert!(Cli::try_parse_from(["difflore", "skills", "sweep"]).is_ok());
}

#[test]
fn public_oss_copy_does_not_reintroduce_cloud_fix_positioning() {
    let surfaces = [
        ("README.md", include_str!("../../../../README.md")),
        (
            "commands/cloud/mod.rs",
            include_str!("../commands/cloud/mod.rs"),
        ),
        (
            "commands/doctor/report/env_probes.rs",
            include_str!("../commands/doctor/report/env_probes.rs"),
        ),
        (
            "difflore-tui/widgets/status_bar.rs",
            include_str!("../../../../crates/difflore-tui/src/widgets/status_bar.rs"),
        ),
        (
            "difflore-tui/modals/fix_runs_low.rs",
            include_str!("../../../../crates/difflore-tui/src/modals/fix_runs_low.rs"),
        ),
    ];
    let forbidden = [
        "hosted auto fix",
        "managed inference",
        "cloud auto fix",
        "auto-fix",
        "auto fix quota",
        "fix-run quota",
        "fix runs",
        "cloud team trial",
        "difflore[bot]",
        "difflore does not run fix locally",
    ];

    for (name, text) in surfaces {
        let lower = text.to_ascii_lowercase();
        for phrase in forbidden {
            assert!(
                !lower.contains(phrase),
                "{name} reintroduced forbidden cloud-fix positioning phrase `{phrase}`"
            );
        }
    }
}

#[test]
fn fix_strict_requires_ci_mode() {
    let mut command = build_cli();
    let fix = command
        .find_subcommand_mut("fix")
        .expect("fix command should exist");
    let help = fix.render_long_help().to_string();
    assert!(help.contains("Requires `--ci`"));

    assert!(
        Cli::try_parse_from(["difflore", "fix", "--strict"]).is_err(),
        "--strict without --ci should fail at parse time"
    );
    assert!(
        Cli::try_parse_from(["difflore", "fix", "--ci", "--strict"]).is_ok(),
        "--strict should be valid with --ci"
    );
    assert!(
        Cli::try_parse_from(["difflore", "fix", "--check", "--strict"]).is_err(),
        "--check alias should not parse"
    );
}

#[test]
fn import_reviews_help_promotes_cli_only_value_path() {
    let mut command = build_cli();
    let import_reviews = command
        .find_subcommand_mut("import-reviews")
        .expect("import-reviews command should exist");
    let help = import_reviews.render_long_help().to_string();

    assert!(help.contains("past GitHub PR review comments"));
    assert!(help.contains("--dry-run"));
    assert!(help.contains("difflore recall --diff"));
    assert!(help.contains("--upload"));
    assert!(!help.contains("difflore candidates"));
    assert!(!help.contains("--local-candidates"));
    assert!(!help.contains("--max-candidates"));
}

#[test]
fn import_reviews_rejects_candidate_budget_flag() {
    assert!(
        Cli::try_parse_from(["difflore", "import-reviews", "--max-candidates", "12"]).is_err(),
        "--max-candidates should not parse"
    );
    assert!(
        Cli::try_parse_from(["difflore", "import-reviews", "--local-candidates"]).is_err(),
        "--local-candidates should not parse"
    );
}

#[test]
fn recall_rejects_eval_escape_hatches() {
    assert!(
        Cli::try_parse_from(["difflore", "recall", "--via", "mcp", "--json"]).is_err(),
        "--via should not parse"
    );
    assert!(
        Cli::try_parse_from(["difflore", "recall", "--copy-agent-context"]).is_err(),
        "--copy-agent-context should not parse"
    );
}

#[test]
fn import_reviews_parses_target_pr_numbers() {
    let cli = Cli::try_parse_from([
        "difflore",
        "import-reviews",
        "--pr",
        "13629",
        "--pr",
        "45397",
        "--json",
    ])
    .expect("import-reviews should parse targeted PR import");

    match cli.command.expect("subcommand") {
        Commands::ImportReviews(args) => {
            assert_eq!(args.pr_numbers, vec![13629, 45397]);
            assert!(args.json);
        }
        _ => panic!("expected import-reviews command"),
    }
}

#[test]
fn import_reviews_parses_exclude_prs_csv() {
    let cli = Cli::try_parse_from(["difflore", "import-reviews", "--exclude-prs", "42,1337,42"])
        .expect("import-reviews should parse --exclude-prs CSV");

    match cli.command.expect("subcommand") {
        Commands::ImportReviews(args) => {
            // clap splits on commas without deduping; dedup happens later during validation.
            assert_eq!(args.exclude_prs, vec![42, 1337, 42]);
        }
        _ => panic!("expected import-reviews command"),
    }
}

#[test]
fn import_reviews_exclude_prs_defaults_to_empty() {
    let cli = Cli::try_parse_from(["difflore", "import-reviews"])
        .expect("import-reviews should parse with no exclusions");

    match cli.command.expect("subcommand") {
        Commands::ImportReviews(args) => {
            assert!(args.exclude_prs.is_empty());
        }
        _ => panic!("expected import-reviews command"),
    }
}
