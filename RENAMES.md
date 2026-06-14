# Module renames

Old → new path mapping for the repo reorganization. Old flat re-exports in
`difflore-core/src/lib.rs` still resolve until the re-export cleanup batch
removes them.

## R1a — difflore-core module moves

| Old | New |
| --- | --- |
| `crates/difflore-core/src/review/` | `crates/difflore-core/src/review_engine/` |
| `crates/difflore-core/src/reviews/` | `crates/difflore-core/src/review_store/` |
| `crates/difflore-core/src/infra/github_import/` | `crates/difflore-core/src/ingest/github/` |
| `crates/difflore-core/src/sources/` | `crates/difflore-core/src/ingest/agent_files/` |
| `crates/difflore-core/src/infra/skill_fs.rs` | `crates/difflore-core/src/skills/fs.rs` |
| `crates/difflore-core/src/domain/settings.rs` | `crates/difflore-core/src/infra/settings.rs` |
| `crates/difflore-core/src/domain/files.rs` | `crates/difflore-core/src/infra/files.rs` |
| `crates/difflore-core/src/observability/observation.rs` | `crates/difflore-core/src/observability/classifier.rs` |

Module path equivalents:

| Old | New |
| --- | --- |
| `difflore_core::review` | `difflore_core::review_engine` |
| `difflore_core::reviews` | `difflore_core::review_store` |
| `difflore_core::github_import` | `difflore_core::ingest::github` |
| `difflore_core::sources` | `difflore_core::ingest::agent_files` |
| `difflore_core::skill_fs` | `difflore_core::skills::fs` |
| `difflore_core::domain::settings` | `difflore_core::infra::settings` |
| `difflore_core::domain::files` | `difflore_core::infra::files` |
| `difflore_core::observation` | `difflore_core::observability::classifier` |

Not executed in R1a:

* `crates/difflore-core/src/migration.rs` — deletion skipped: `run_if_needed`
  has a live caller in `crates/difflore-cli/src/lib.rs` (startup guard) plus
  integration coverage in `crates/difflore-core/tests/migration_test.rs`.

## R1b — contract split, error extraction, mcp_server slimming

File moves:

| Old | New |
| --- | --- |
| `crates/difflore-core/openapi-spec.json` | `crates/difflore-core/contracts/openapi-spec.json` (provenance in `contracts/SOURCE`) |
| `crates/difflore-core/src/cloud/api_types.rs` | `crates/difflore-core/src/contract/{mod,generated,dto}.rs` (generated/hand-written split) |
| `crates/difflore-core/src/domain/mod.rs` (`CoreError` + `errors` shim) | `crates/difflore-core/src/error.rs` |
| `crates/difflore-core/src/mcp_server/mod.rs` (`predict_pr_scope*`) | `crates/difflore-core/src/mcp_server/pr_scope.rs` |
| `crates/difflore-core/src/mcp_server/tools/util.rs` | `crates/difflore-core/src/mcp_server/tools/{validate,evidence,serve_stats}.rs` |

Module path equivalents (the old flat re-exports in `lib.rs` were deleted in
this batch — the old paths below no longer resolve; bumped to 0.2.0):

| Old | New |
| --- | --- |
| `difflore_core::cloud::api_types` | `difflore_core::contract` |
| `difflore_core::errors` | `difflore_core::error` |
| `difflore_core::models` | `difflore_core::domain::models` |
| `difflore_core::origins` | `difflore_core::domain::origins` |
| `difflore_core::projects` | `difflore_core::domain::projects` |
| `difflore_core::providers` | `difflore_core::domain::providers` |
| `difflore_core::rule_view` | `difflore_core::domain::rule_view` |
| `difflore_core::activity_stream` | `difflore_core::observability::activity_stream` |
| `difflore_core::cost` | `difflore_core::observability::cost` |
| `difflore_core::fix_outcomes` | `difflore_core::observability::fix_outcomes` |
| `difflore_core::injection_log` | `difflore_core::observability::injection_log` |
| `difflore_core::mcp_rule_serves` | `difflore_core::observability::mcp_rule_serves` |
| `difflore_core::privacy` | `difflore_core::observability::privacy` |
| `difflore_core::rule_outcomes` | `difflore_core::observability::rule_outcomes` |
| `difflore_core::stated_vs_actual` | `difflore_core::observability::stated_vs_actual` |
| `difflore_core::review_trajectory` | `difflore_core::observability::trajectory` |
| `difflore_core::config` | `difflore_core::infra::config` |
| `difflore_core::crypto` | `difflore_core::infra::crypto` |
| `difflore_core::daemon` | `difflore_core::infra::daemon` |
| `difflore_core::db` | `difflore_core::infra::db` |
| `difflore_core::env` | `difflore_core::infra::env` |
| `difflore_core::files` | `difflore_core::infra::files` |
| `difflore_core::git` | `difflore_core::infra::git` |
| `difflore_core::paths` | `difflore_core::infra::paths` |
| `difflore_core::settings` | `difflore_core::infra::settings` |
| `difflore_core::startup` | `difflore_core::infra::startup` |

Kept at the crate root (type re-exports, not module aliases):
`difflore_core::CoreError`, `difflore_core::Result`,
`difflore_core::SqlitePool`.

## R2 — difflore-cli crate moves

| Old | New |
| --- | --- |
| `crates/difflore-cli/src/hooks/` (adapter trait + per-client adapters) | `crates/difflore-cli/src/hook/adapters/` |
| `crates/difflore-cli/src/hooks/session_banner/` | `crates/difflore-cli/src/hook/banner/` |
| `crates/difflore-cli/src/hook_runtime/` | `crates/difflore-cli/src/hook/runtime/` |
| `crates/difflore-cli/src/hook_runtime/stated_vs_actual.rs` | `crates/difflore-cli/src/hook/runtime/drift_report.rs` |
| `crates/difflore-cli/src/hook_cache.rs` | `crates/difflore-cli/src/hook/cache.rs` |
| `crates/difflore-cli/src/hook_forward.rs` | `crates/difflore-cli/src/hook/forward/mod.rs` (+ new `protocol.rs`, wire shapes the `difflore-hook` bin now reuses) |
| `crates/difflore-cli/src/mcp_install/` | `crates/difflore-cli/src/installer/` |
| `crates/difflore-cli/src/agent_cli/` | `crates/difflore-cli/src/agent_exec/` |
| `crates/difflore-cli/src/dispatch/mod.rs` | `crates/difflore-cli/src/dispatch.rs` |
| `crates/difflore-cli/src/runtime/{mod,context}.rs` | `crates/difflore-cli/src/runtime.rs` |
| `crates/difflore-cli/src/commands/util.rs` | `crates/difflore-cli/src/support/util.rs` |
| `crates/difflore-cli/src/commands/review_text.rs` | `crates/difflore-cli/src/support/review_text.rs` |
| `crates/difflore-cli/src/commands/impact_payload.rs` | `crates/difflore-cli/src/support/impact_payload.rs` |
| `crates/difflore-cli/src/commands/welcome.rs` | `crates/difflore-cli/src/onboarding.rs` |
| `crates/difflore-cli/src/commands/sync.rs` | `crates/difflore-cli/src/commands/cloud/sync.rs` |
| `crates/difflore-cli/src/commands/search.rs` | `crates/difflore-cli/src/commands/recall/search.rs` |
| `crates/difflore-cli/src/commands/audit_history.rs` | `crates/difflore-cli/src/commands/doctor/audit_history.rs` |
| `crates/difflore-cli/src/commands/path_hints.rs` | `crates/difflore-cli/src/commands/fix/path_hints.rs` |
| `crates/difflore-cli/src/commands/import_reviews.rs` | `crates/difflore-cli/src/commands/import_reviews/mod.rs` |

New (no old path): `crates/difflore-cli/src/clients.rs` (`ClientId`, the
single compile-time client table the installer registry / hook adapters /
agent_exec all match over).

Module path equivalents (crate-internal; `difflore-cli` is a binary crate so
no published API surface changes):

| Old | New |
| --- | --- |
| `difflore_cli::hooks` | `difflore_cli::hook::adapters` |
| `difflore_cli::hooks::session_banner` | `difflore_cli::hook::banner` |
| `difflore_cli::hook_runtime` | `difflore_cli::hook::runtime` |
| `difflore_cli::hook_cache` | `difflore_cli::hook::cache` |
| `difflore_cli::hook_forward` | `difflore_cli::hook::forward` |
| `difflore_cli::mcp_install` | `difflore_cli::installer` |
| `difflore_cli::agent_cli` | `difflore_cli::agent_exec` |
| `difflore_cli::commands::{util,review_text,impact_payload}` | `difflore_cli::support::{util,review_text,impact_payload}` |
| `difflore_cli::commands::welcome` | `difflore_cli::onboarding` |

## R3 — difflore-tui reorg + TUI entry wiring

| Old | New |
| --- | --- |
| `crates/difflore-tui/src/state.rs` | `crates/difflore-tui/src/plan.rs` |
| `crates/difflore-tui/src/app/state.rs` | `crates/difflore-tui/src/app/selectors.rs` |
| `crates/difflore-tui/src/app/mod.rs` (terminal lifecycle) | `crates/difflore-tui/src/app/terminal.rs` |
| `crates/difflore-tui/src/app/mod.rs` (plan build / cloud mapping) | `crates/difflore-tui/src/app/plan_state.rs` |
| `crates/difflore-tui/src/app/mod.rs` (Rules filter enums + origin counts) | `crates/difflore-tui/src/tabs/memory/filter.rs` |
| `crates/difflore-tui/src/app/mod.rs` (`origin_color`) | `crates/difflore-tui/src/theme/mod.rs` |
| `crates/difflore-tui/src/app/modals.rs` | `crates/difflore-tui/src/modals/dispatch.rs` (per-modal keymaps live in each `modals/<name>.rs`) |
| `crates/difflore-tui/src/layout.rs` | `crates/difflore-tui/src/widgets/center.rs` |
| `crates/difflore-tui/src/widgets/mod.rs` (`truncate`) | `crates/difflore-tui/src/widgets/text.rs` |
| `crates/difflore-tui/src/theme.rs` | `crates/difflore-tui/src/theme/{mod.rs,source.rs}` (palette vs config IO + mtime cache) |
| `crates/difflore-tui/src/tabs/rules/` | `crates/difflore-tui/src/tabs/memory/` |
| `crates/difflore-tui/src/tabs/activity.rs` | `crates/difflore-tui/src/tabs/fixes.rs` |
| `crates/difflore-tui/src/tabs/team.rs` | `crates/difflore-tui/src/tabs/cloud.rs` |
| `crates/difflore-tui/src/tabs/settings.rs` | `crates/difflore-tui/src/tabs/setup.rs` |

Tab enum variants follow the product vocabulary: `Tab::{Rules,Activity,Team,Settings}`
→ `Tab::{Memory,Fixes,Cloud,Setup}`. Published-API module path changes
(`difflore-tui` 0.2.0, unreleased): `difflore_tui::state` → `difflore_tui::plan`.

New (no old path): `crates/difflore-cli/src/tui_entry.rs` — bare `difflore`
glue that builds the `WiringSnapshot`, launches the dashboard after the
welcome flow, and maps `TuiExit` back onto the dispatch table.

## R5 — hook-forward daemon wiring

No renames; new code only. `crates/difflore-cli/src/hook/forward/spawn.rs` —
OS-level detached spawn of the warm daemon (Unix `setsid`, Windows
`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`). `forward::run_server` was
replaced by `forward::run_server_for_hash(project_hash)` (the daemon's index
pool is now selected by an explicit hash, not the daemon's cwd). New hidden CLI
subcommand `difflore __hook-daemon --project-hash <hash>` (`Commands::HookDaemon`).
