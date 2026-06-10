# Changelog

All notable changes to DiffLore are listed here. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-06-10

### Changed

- **Breaking (library API of `difflore-core`):** internal module reorganization
  (reorg batches R1a + R1b). The old flat top-level re-exports in
  `difflore_core` (e.g. `difflore_core::models`, `difflore_core::db`,
  `difflore_core::env`) and the `review_trajectory` alias were removed —
  one module, one name. Full old → new mapping lives in `RENAMES.md`;
  the highlights:
  - `difflore_core::review` → `difflore_core::review_engine`
  - `difflore_core::reviews` → `difflore_core::review_store`
  - `difflore_core::sources` → `difflore_core::ingest::agent_files`
  - `difflore_core::github_import` → `difflore_core::ingest::github`
  - `difflore_core::skill_fs` → `difflore_core::skills::fs`
  - `difflore_core::observation` → `difflore_core::observability::classifier`
  - `difflore_core::review_trajectory` → `difflore_core::observability::trajectory`
  - `difflore_core::models` / `origins` / `projects` / `providers` /
    `rule_view` → `difflore_core::domain::*`
  - `difflore_core::activity_stream` / `cost` / `fix_outcomes` /
    `injection_log` / `mcp_rule_serves` / `privacy` / `rule_outcomes` /
    `stated_vs_actual` → `difflore_core::observability::*`
  - `difflore_core::config` / `crypto` / `daemon` / `db` / `env` / `files` /
    `git` / `paths` / `settings` / `startup` → `difflore_core::infra::*`
  - `difflore_core::errors` → `difflore_core::error` (`CoreError` moved out
    of `domain` into a crate-level `src/error.rs`; the root
    `difflore_core::CoreError` / `difflore_core::Result` re-exports remain)
  - `difflore_core::cloud::api_types` → `difflore_core::contract`
    (split into `contract::generated` — the `generate_types!` output — and
    `contract::dto` — hand-written DTOs for endpoints outside the spec)
- The vendored OpenAPI spec moved from the `difflore-core` crate root to
  `crates/difflore-core/contracts/openapi-spec.json`, with provenance pinned
  in `contracts/SOURCE`.
- The CLI commands and on-disk formats are unchanged; this release is a
  library-API version bump only.

### Added

- `scripts/layer-gate.sh`: structural lint asserting no orphan source
  directories and a pure domain layer (CI wiring follows in a later batch).

## [0.1.0] - 2026-06-08

### Added

- First public release of the local-first DiffLore CLI.
- GitHub PR review import with `difflore import-reviews`, including dry-run
  planning and repo-scoped rule attribution.
- Local review-memory recall with `difflore recall`, `difflore ask`, and
  installed MCP tools for supported AI coding agents.
- Agent wiring with `difflore agents install`, `agents status`, `agents update`,
  and `agents uninstall`.
- Rule-aware local fix previews with `difflore fix --preview`; accepted changes
  only touch the local working tree.
- Local status and diagnostics through bare `difflore`, `difflore status`, and
  `difflore doctor --report`.
- Optional semantic recall configuration through `difflore embeddings setup`.
- Optional cloud login and sync commands for teams that want shared memory,
  governance, and impact views.
- Public documentation for installation, CLI usage, security reporting,
  contribution workflow, and release notes.
