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
