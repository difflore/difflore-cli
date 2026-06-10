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
