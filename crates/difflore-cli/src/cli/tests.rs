use super::{
    AgentsCommands, AuthCommands, Cli, CloudCommands, Commands, DraftsCommands, ExportFormatArg,
    ImportDistillArg, MemoryCommands, MemoryPackageFormatArg, StatusLane, build_cli,
};
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
        "  try",
        "  init",
        "  import-reviews",
        "  memory",
        "  learn",
        "  recall",
        "  review",
        "  fix",
        "  ask",
        "  export",
        "  cloud",
        "  agents",
        "  providers",
        "  embeddings",
        "  auth",
        "  doctor",
        "  update",
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
        "  status",
        "  drafts",
    ] {
        assert!(!help.contains(removed), "{removed} should not be visible");
    }

    assert!(help.contains("Choose the local AI backend"));
    assert!(help.contains("Tune semantic recall quality"));
    assert!(help.contains("Store GitLab import credentials"));
    assert!(help.contains("Diagnose installs, hooks, sync, and recall"));
    assert!(help.contains("Refresh installed agent blocks and run diagnostics"));
    assert!(help.contains("Optional: sync"));
    assert!(help.contains("New here?"));
}

#[test]
fn learn_command_parses_note_transcript_session_and_json() {
    let parsed = Cli::try_parse_from([
        "difflore",
        "learn",
        "--note",
        "Prefer generated clients",
        "--transcript",
        "/tmp/session.jsonl",
        "--session",
        "sess-1",
        "--json",
    ])
    .expect("learn should parse");

    let Some(Commands::Learn(args)) = parsed.command else {
        panic!("expected learn command");
    };
    assert_eq!(args.note.as_deref(), Some("Prefer generated clients"));
    assert_eq!(
        args.transcript.as_deref(),
        Some(std::path::Path::new("/tmp/session.jsonl"))
    );
    assert_eq!(args.session.as_deref(), Some("sess-1"));
    assert!(args.json);
}

#[test]
fn memory_command_parses_summary_inbox_active_activity_show_review_actions_sync_and_drafts_alias() {
    let summary = Cli::try_parse_from(["difflore", "memory"]).expect("memory should parse");
    assert!(matches!(
        summary.command,
        Some(Commands::Memory {
            json: false,
            command: None,
        })
    ));

    let summary_json =
        Cli::try_parse_from(["difflore", "memory", "--json"]).expect("memory --json should parse");
    assert!(matches!(
        summary_json.command,
        Some(Commands::Memory {
            json: true,
            command: None,
        })
    ));

    let inbox =
        Cli::try_parse_from(["difflore", "memory", "inbox", "--json"]).expect("inbox parses");
    assert!(matches!(
        inbox.command,
        Some(Commands::Memory {
            json: false,
            command: Some(MemoryCommands::Inbox {
                all: false,
                limit: None,
                json: true
            }),
        })
    ));

    let inbox_all = Cli::try_parse_from(["difflore", "memory", "inbox", "--all", "--json"])
        .expect("inbox --all parses");
    assert!(matches!(
        inbox_all.command,
        Some(Commands::Memory {
            json: false,
            command: Some(MemoryCommands::Inbox {
                all: true,
                limit: None,
                json: true
            }),
        })
    ));

    let inbox_limited = Cli::try_parse_from(["difflore", "memory", "inbox", "--limit", "20"])
        .expect("inbox --limit parses");
    assert!(matches!(
        inbox_limited.command,
        Some(Commands::Memory {
            json: false,
            command: Some(MemoryCommands::Inbox {
                all: false,
                limit: Some(20),
                json: false
            }),
        })
    ));

    let active = Cli::try_parse_from(["difflore", "memory", "active", "--limit", "25", "--json"])
        .expect("memory active parses");
    assert!(matches!(
        active.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Active {
                all: false,
                limit: Some(25),
                json: true
            }),
            ..
        })
    ));

    let active_all =
        Cli::try_parse_from(["difflore", "memory", "active", "--all", "--limit", "25"])
            .expect("memory active --all parses");
    assert!(matches!(
        active_all.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Active {
                all: true,
                limit: Some(25),
                json: false
            }),
            ..
        })
    ));

    let activity = Cli::try_parse_from([
        "difflore", "memory", "activity", "--days", "7", "--limit", "10", "--json",
    ])
    .expect("memory activity parses");
    assert!(matches!(
        activity.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Activity {
                days: 7,
                limit: Some(10),
                json: true
            }),
            ..
        })
    ));

    let show = Cli::try_parse_from([
        "difflore",
        "memory",
        "show",
        "session:abc123def4567890",
        "--json",
    ])
    .expect("memory show parses");
    assert!(matches!(
        show.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Show { item_id, json: true }),
            ..
        }) if item_id == "session:abc123def4567890"
    ));

    let show_rule = Cli::try_parse_from(["difflore", "memory", "show", "rule:conv-abc12345"])
        .expect("memory show rule parses");
    assert!(matches!(
        show_rule.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Show { item_id, json: false }),
            ..
        }) if item_id == "rule:conv-abc12345"
    ));

    let show_draft = Cli::try_parse_from(["difflore", "memory", "show", "draft:conv-abc12345"])
        .expect("memory show draft parses");
    assert!(matches!(
        show_draft.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Show { item_id, json: false }),
            ..
        }) if item_id == "draft:conv-abc12345"
    ));

    let remember = Cli::try_parse_from([
        "difflore",
        "memory",
        "remember",
        "--title",
        "Flatten single-component directories",
        "--body",
        "If a directory only contains one component, flatten it to the parent.",
        "--file-pattern",
        "**/*.tsx",
        "--file-pattern",
        "**/*.module.css",
        "--json",
    ])
    .expect("memory remember parses");
    assert!(matches!(
        remember.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Remember {
                title,
                body: Some(body),
                file_patterns,
                json: true,
                ..
            }),
            ..
        }) if title == "Flatten single-component directories"
            && body.contains("flatten it")
            && file_patterns == vec!["**/*.tsx".to_owned(), "**/*.module.css".to_owned()]
    ));

    let import_agent_files =
        Cli::try_parse_from(["difflore", "memory", "import-agent-files", "--json"])
            .expect("memory import-agent-files parses");
    assert!(matches!(
        import_agent_files.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::ImportAgentFiles { json: true }),
            ..
        })
    ));

    let review = Cli::try_parse_from(["difflore", "memory", "review", "--limit", "10"])
        .expect("review parses");
    assert!(matches!(
        review.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Review { limit: Some(10) }),
            ..
        })
    ));

    let autopilot = Cli::try_parse_from([
        "difflore",
        "memory",
        "autopilot",
        "--dry-run",
        "--max-auto-enable",
        "4",
        "--json",
    ])
    .expect("memory autopilot parses");
    assert!(matches!(
        autopilot.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Autopilot {
                dry_run: true,
                max_auto_enable: Some(4),
                json: true,
                background: false,
                lease_owner: None,
            }),
            ..
        })
    ));

    let digest = Cli::try_parse_from(["difflore", "memory", "digest", "--limit", "12", "--json"])
        .expect("memory digest parses");
    assert!(matches!(
        digest.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Digest {
                limit: Some(12),
                json: true,
            }),
            ..
        })
    ));

    let recommended = Cli::try_parse_from([
        "difflore",
        "memory",
        "recommended",
        "--limit",
        "7",
        "--json",
    ])
    .expect("memory recommended parses");
    assert!(matches!(
        recommended.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Recommended {
                all: false,
                limit: Some(7),
                json: true,
                approve: false,
                yes: false,
            }),
            ..
        })
    ));

    let log = Cli::try_parse_from(["difflore", "memory", "log", "--limit", "8", "--json"])
        .expect("memory log parses");
    assert!(matches!(
        log.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Log {
                limit: Some(8),
                json: true,
            }),
            ..
        })
    ));

    let disable = Cli::try_parse_from([
        "difflore",
        "memory",
        "disable",
        "rule:conv-abc12345",
        "--reason",
        "too noisy",
        "--json",
    ])
    .expect("memory disable parses");
    assert!(matches!(
        disable.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Disable {
                rule_id,
                reason: Some(reason),
                json: true,
            }),
            ..
        }) if rule_id == "rule:conv-abc12345" && reason == "too noisy"
    ));

    let approve = Cli::try_parse_from([
        "difflore",
        "memory",
        "approve",
        "session:abc123def4567890",
        "--json",
    ])
    .expect("approve parses");
    assert!(matches!(
        approve.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Approve { item_id, json: true }),
            ..
        }) if item_id == "session:abc123def4567890"
    ));

    let reject = Cli::try_parse_from([
        "difflore",
        "memory",
        "reject",
        "session:abc123def4567890",
        "--json",
    ])
    .expect("reject parses");
    assert!(matches!(
        reject.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Reject { item_id, json: true }),
            ..
        }) if item_id == "session:abc123def4567890"
    ));

    let approve_draft =
        Cli::try_parse_from(["difflore", "memory", "approve", "draft:cand-1", "--json"])
            .expect("draft approve parses through memory");
    assert!(matches!(
        approve_draft.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Approve { item_id, json: true }),
            ..
        }) if item_id == "draft:cand-1"
    ));

    let reject_draft =
        Cli::try_parse_from(["difflore", "memory", "reject", "draft:cand-1", "--json"])
            .expect("draft reject parses through memory");
    assert!(matches!(
        reject_draft.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Reject { item_id, json: true }),
            ..
        }) if item_id == "draft:cand-1"
    ));

    let sync = Cli::try_parse_from(["difflore", "memory", "sync", "--json"]).expect("sync parses");
    assert!(matches!(
        sync.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Sync(args)),
            ..
        }) if args.json
            && !args.include_observations
            && !args.include_candidates
            && !args.include_telemetry
    ));

    let sync_with_raw = Cli::try_parse_from([
        "difflore",
        "memory",
        "sync",
        "--include-observations",
        "--include-candidates",
        "--include-telemetry",
    ])
    .expect("memory sync raw include flags parse");
    assert!(matches!(
        sync_with_raw.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Sync(args)),
            ..
        }) if args.include_observations && args.include_candidates && args.include_telemetry
    ));

    let export_package = Cli::try_parse_from([
        "difflore",
        "memory",
        "export-package",
        "--output",
        "memory-package",
        "--format",
        "markdown",
        "--dry-run",
        "--json",
        "--local-only",
        "--max-rules",
        "10",
    ])
    .expect("memory export-package parses");
    assert!(matches!(
        export_package.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::ExportPackage {
                output,
                format: MemoryPackageFormatArg::Markdown,
                dry_run: true,
                json: true,
                local_only: true,
                max_rules: Some(10),
            }),
            ..
        }) if output == *"memory-package"
    ));

    let import_package = Cli::try_parse_from([
        "difflore",
        "memory",
        "import-package",
        "--source",
        "memory-package",
        "--dry-run",
        "--json",
    ])
    .expect("memory import-package parses");
    assert!(matches!(
        import_package.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::ImportPackage {
                source,
                dry_run: true,
                json: true,
            }),
            ..
        }) if source == *"memory-package"
    ));

    let drafts = Cli::try_parse_from(["difflore", "memory", "drafts", "list", "--json"])
        .expect("memory drafts list parses");
    assert!(matches!(
        drafts.command,
        Some(Commands::Memory {
            command: Some(MemoryCommands::Drafts {
                command: DraftsCommands::List { json: true, .. }
            }),
            ..
        })
    ));
}

#[test]
fn drafts_command_parses_review_and_batch_actions() {
    let review = Cli::try_parse_from(["difflore", "drafts", "review", "--repo", "Acme/App"])
        .expect("drafts review should parse");
    assert!(matches!(
        review.command,
        Some(Commands::Drafts {
            command: DraftsCommands::Review {
                repo: Some(repo),
                limit: None,
            }
        }) if repo == "Acme/App"
    ));

    let list = Cli::try_parse_from(["difflore", "drafts", "list", "--limit", "5", "--json"])
        .expect("drafts list should parse");
    assert!(matches!(
        list.command,
        Some(Commands::Drafts {
            command: DraftsCommands::List {
                repo: None,
                limit: Some(5),
                json: true,
            }
        })
    ));

    let approve_all = Cli::try_parse_from([
        "difflore", "drafts", "approve", "--all", "--repo", "acme/app", "--yes", "--json",
    ])
    .expect("drafts approve --all should parse");
    assert!(matches!(
        approve_all.command,
        Some(Commands::Drafts {
            command: DraftsCommands::Approve {
                id: None,
                all: true,
                repo: Some(repo),
                yes: true,
                json: true,
            }
        }) if repo == "acme/app"
    ));

    let reject_one = Cli::try_parse_from(["difflore", "drafts", "reject", "draft-1"])
        .expect("drafts reject should parse");
    assert!(matches!(
        reject_one.command,
        Some(Commands::Drafts {
            command: DraftsCommands::Reject {
                id: Some(id),
                all: false,
                repo: None,
                yes: false,
                json: false,
            }
        }) if id == "draft-1"
    ));
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
fn doctor_report_accepts_default_stdout_and_path_targets() {
    let default_report =
        Cli::try_parse_from(["difflore", "doctor", "--report"]).expect("report flag");
    assert!(matches!(
        default_report.command,
        Some(Commands::Doctor {
            report: Some(ref target),
            ..
        }) if target.is_empty()
    ));

    let stdout_report =
        Cli::try_parse_from(["difflore", "doctor", "--report", "-"]).expect("stdout report");
    assert!(matches!(
        stdout_report.command,
        Some(Commands::Doctor {
            report: Some(ref target),
            ..
        }) if target == "-"
    ));

    let file_report =
        Cli::try_parse_from(["difflore", "doctor", "--report", "report.md"]).expect("file report");
    assert!(matches!(
        file_report.command,
        Some(Commands::Doctor {
            report: Some(ref target),
            ..
        }) if target == "report.md"
    ));

    let report_with_fix =
        Cli::try_parse_from(["difflore", "doctor", "--report", "--fix"]).expect("report + fix");
    assert!(matches!(
        report_with_fix.command,
        Some(Commands::Doctor {
            report: Some(ref target),
            fix: true,
            ..
        }) if target.is_empty()
    ));
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
fn fix_pr_work_branch_conflicts_with_no_checkout() {
    let args = [
        "difflore",
        "fix",
        "--pr",
        "difflore/difflore-cli#42",
        "--work-branch",
        "local-pr-42",
        "--no-checkout",
    ];
    assert!(
        Cli::try_parse_from(args).is_err(),
        "work branch and no-checkout should conflict"
    );
}

#[test]
fn fix_no_longer_accepts_read_only_review_modes() {
    for args in [
        ["difflore", "fix", "--preview"].as_slice(),
        ["difflore", "fix", "--ci"].as_slice(),
        ["difflore", "fix", "--strict"].as_slice(),
        ["difflore", "fix", "--json"].as_slice(),
    ] {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "{args:?} should not parse on fix"
        );
    }
    assert!(Cli::try_parse_from(["difflore", "fix", "--yes", "--json"]).is_ok());
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

    let update_all = Cli::try_parse_from(["difflore", "update", "--dry-run", "--force"])
        .expect("top-level update should parse");
    assert!(matches!(
        update_all.command,
        Some(Commands::Update {
            dry_run: true,
            force: true
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

    let capabilities = Cli::try_parse_from(["difflore", "capabilities", "--json"])
        .expect("capabilities should parse");
    assert!(matches!(
        capabilities.command,
        Some(Commands::Capabilities { json: true })
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
            command: CloudCommands::Sync(super::SyncCliArgs {
                dry_run: true,
                include_observations: false,
                include_candidates: false,
                include_telemetry: false,
                ..
            })
        })
    ));

    let raw_sync = Cli::try_parse_from([
        "difflore",
        "cloud",
        "sync",
        "--include-observations",
        "--include-candidates",
        "--include-telemetry",
    ])
    .expect("cloud sync raw include flags should parse");
    assert!(matches!(
        raw_sync.command,
        Some(Commands::Cloud {
            command: CloudCommands::Sync(super::SyncCliArgs {
                include_observations: true,
                include_candidates: true,
                include_telemetry: true,
                ..
            })
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
fn auth_gitlab_parses_default_host_check_and_remove_modes() {
    let store = Cli::try_parse_from(["difflore", "auth", "gitlab"])
        .expect("bare auth gitlab should parse (token arrives via stdin/env)");
    assert!(matches!(
        store.command,
        Some(Commands::Auth {
            command: AuthCommands::Gitlab { host, check: false, remove: false }
        }) if host == "gitlab.com"
    ));

    let check = Cli::try_parse_from([
        "difflore",
        "auth",
        "gitlab",
        "--check",
        "--host",
        "gitlab.corp.example",
    ])
    .expect("auth gitlab --check --host should parse");
    assert!(matches!(
        check.command,
        Some(Commands::Auth {
            command: AuthCommands::Gitlab { host, check: true, remove: false }
        }) if host == "gitlab.corp.example"
    ));

    let remove = Cli::try_parse_from(["difflore", "auth", "gitlab", "--remove"])
        .expect("auth gitlab --remove should parse");
    assert!(matches!(
        remove.command,
        Some(Commands::Auth {
            command: AuthCommands::Gitlab {
                check: false,
                remove: true,
                ..
            }
        })
    ));

    assert!(
        Cli::try_parse_from(["difflore", "auth", "gitlab", "--check", "--remove"]).is_err(),
        "--check and --remove are distinct modes and must conflict"
    );

    // No --token flag by design: tokens arrive via stdin or env so they never
    // land in shell history.
    assert!(
        Cli::try_parse_from(["difflore", "auth", "gitlab", "--token", "glpat-x"]).is_err(),
        "auth gitlab must not accept a --token flag"
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
        "mcp",
        "lsp",
        "migrate",
        "demo",
        "tui",
        "dist",
        "daemon",
        "rules",
        "sync",
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

    // The internal warm hook daemon parses (the shim spawns it by this exact
    // invocation) but stays out of help — the double-underscore name marks it
    // internal and `hide = true` keeps it off the curated surface.
    assert!(!help.contains("__hook-daemon"));
    let daemon = Cli::try_parse_from(["difflore", "__hook-daemon", "--project-hash", "abc123"])
        .expect("hidden hook daemon should parse for the shim spawn");
    assert!(matches!(
        daemon.command,
        Some(Commands::HookDaemon { project_hash }) if project_hash == "abc123"
    ));
    // It requires the hash — a bare invocation must not silently serve the
    // wrong (cwd-derived) project.
    assert!(Cli::try_parse_from(["difflore", "__hook-daemon"]).is_err());

    assert!(!help.contains("__outbox-daemon"));
    let outbox_daemon = Cli::try_parse_from(["difflore", "__outbox-daemon"])
        .expect("hidden outbox daemon should parse for self-managed spawn");
    assert!(matches!(
        outbox_daemon.command,
        Some(Commands::OutboxDaemon {
            tick_interval_secs: 5,
            batch_size: 64,
        })
    ));
}

#[test]
fn public_oss_copy_does_not_reintroduce_cloud_fix_positioning() {
    let surfaces = [
        ("README.md", include_str!("../../../../README.md")),
        ("CHANGELOG.md", include_str!("../../../../CHANGELOG.md")),
        (
            "commands/cloud/mod.rs",
            include_str!("../commands/cloud/mod.rs"),
        ),
        (
            "commands/doctor/report/env_probes.rs",
            include_str!("../commands/doctor/report/env_probes.rs"),
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
fn public_docs_do_not_advertise_removed_tui_or_rules_remember_commands() {
    let docs = [
        ("README.md", include_str!("../../../../README.md")),
        ("CHANGELOG.md", include_str!("../../../../CHANGELOG.md")),
    ];
    let removed_examples = ["difflore tui", "difflore rules remember"];

    for (name, text) in docs {
        let lower = text.to_ascii_lowercase();
        for example in removed_examples {
            assert!(
                !lower.contains(example),
                "{name} should not advertise removed command example `{example}`"
            );
        }
    }
}

#[test]
fn review_strict_requires_ci_mode() {
    let mut command = build_cli();
    let review = command
        .find_subcommand_mut("review")
        .expect("review command should exist");
    let help = review.render_long_help().to_string();
    assert!(help.contains("Requires `--ci`"));

    assert!(
        Cli::try_parse_from(["difflore", "review", "--strict"]).is_err(),
        "--strict without --ci should fail at parse time"
    );
    assert!(
        Cli::try_parse_from(["difflore", "review", "--ci", "--strict"]).is_ok(),
        "--strict should be valid with --ci"
    );
    assert!(
        Cli::try_parse_from(["difflore", "review", "--check", "--strict"]).is_err(),
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

    assert!(help.contains("past GitHub PR or GitLab MR review comments"));
    assert!(help.contains("--dry-run"));
    assert!(help.contains("difflore recall --diff"));
    assert!(help.contains("--upload"));
    // GitLab surface: provider flags exist and `--pr` explains its MR-IID
    // meaning so the flag reuse is discoverable from --help alone.
    assert!(help.contains("--provider"));
    assert!(help.contains("--gitlab-host"));
    assert!(help.contains("--distill"));
    assert!(help.contains("MR IID"));
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
fn import_reviews_parses_local_agent_distill_and_rejects_upload_conflict() {
    let default_cli = Cli::try_parse_from(["difflore", "import-reviews"])
        .expect("import-reviews should parse with default distill");
    match default_cli.command.expect("subcommand") {
        Commands::ImportReviews(args) => {
            assert_eq!(args.distill, ImportDistillArg::Auto);
        }
        _ => panic!("expected import-reviews command"),
    }

    let upload_cli = Cli::try_parse_from(["difflore", "import-reviews", "--upload"])
        .expect("--upload should remain explicit and valid with default auto distill");
    match upload_cli.command.expect("subcommand") {
        Commands::ImportReviews(args) => {
            assert!(args.upload);
            assert_eq!(args.distill, ImportDistillArg::Auto);
        }
        _ => panic!("expected import-reviews command"),
    }

    let cli = Cli::try_parse_from(["difflore", "import-reviews", "--distill", "local-agent"])
        .expect("import-reviews should parse local-agent distill");

    match cli.command.expect("subcommand") {
        Commands::ImportReviews(args) => {
            assert_eq!(args.distill, ImportDistillArg::LocalAgent);
        }
        _ => panic!("expected import-reviews command"),
    }

    assert!(
        Cli::try_parse_from([
            "difflore",
            "import-reviews",
            "--distill",
            "local-agent",
            "--upload"
        ])
        .is_err(),
        "--distill local-agent should conflict with --upload"
    );
}

#[test]
fn import_reviews_parses_hidden_wall_timeout_for_post_install() {
    let cli = Cli::try_parse_from(["difflore", "import-reviews", "--wall-timeout-secs", "20"])
        .expect("hidden post-install timeout should parse");

    match cli.command.expect("subcommand") {
        Commands::ImportReviews(args) => {
            assert_eq!(args.wall_timeout_secs, Some(20));
        }
        _ => panic!("expected import-reviews command"),
    }
}

#[test]
fn export_parses_formats_and_safety_flags() {
    let cli = Cli::try_parse_from([
        "difflore",
        "export",
        "--format",
        "claude-md",
        "--format",
        "agents-md",
        "--dry-run",
        "--no-examples",
        "--local-only",
        "--json",
    ])
    .expect("export should parse repeated formats and flags");

    match cli.command.expect("subcommand") {
        Commands::Export(args) => {
            assert_eq!(
                args.format,
                vec![ExportFormatArg::ClaudeMd, ExportFormatArg::AgentsMd]
            );
            assert!(args.dry_run);
            assert!(args.no_examples);
            assert!(args.local_only);
            assert!(args.json);
        }
        _ => panic!("expected export command"),
    }
}

#[test]
fn export_defaults_to_all_formats_and_writing() {
    let cli = Cli::try_parse_from(["difflore", "export"]).expect("bare export should parse");
    match cli.command.expect("subcommand") {
        Commands::Export(args) => {
            assert_eq!(args.format, vec![ExportFormatArg::All]);
            assert!(!args.dry_run);
            assert!(!args.no_examples);
            assert!(!args.local_only);
            assert!(!args.json);
        }
        _ => panic!("expected export command"),
    }
}

#[test]
fn export_help_states_static_snapshot_and_side_effects() {
    let mut command = build_cli();
    let export = command
        .find_subcommand_mut("export")
        .expect("export command should exist");
    let help = export.render_long_help().to_string();

    // The export is honest about being a stale-able snapshot and points at
    // the live path; the side-effect contract names the marker boundary.
    assert!(help.contains("static snapshot"));
    assert!(help.contains("goes stale"));
    assert!(help.contains("difflore agents install"));
    assert!(help.contains("BEGIN/END DIFFLORE RULES"));
    assert!(help.contains("never commits"));
    assert!(help.contains(".gitignore"));
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
