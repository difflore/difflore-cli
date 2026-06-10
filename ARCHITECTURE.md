# Architecture & Conventions

Cargo workspace, edition 2024. Three crates under `crates/`:

| Crate | Role | Depends on |
| --- | --- | --- |
| `difflore-core` | Cloud API client, context retrieval, rule storage, MCP server, observability | — |
| `difflore-tui` | Interactive terminal UI | core |
| `difflore-cli` | CLI entry point, command dispatch, hooks runtime, MCP install | core, tui |

Publish/release order follows that dependency chain.

## Conventions

- Modules and files: `snake_case`, per Rust convention. Keep `main.rs` a thin shim over `lib.rs`.
- Large commands live as directories with focused submodules (see `commands/fix/`, `commands/doctor/`).
- Workspace lints are strict (`unwrap`/`todo`/`unimplemented` denied); all crates inherit `[lints] workspace = true`.
- Tests: unit tests in `#[cfg(test)]` modules. A module's tests live either inline or in a sibling `tests.rs` / `tests/` directory next to the code (e.g. `mcp_server/tests/`, `team/tests.rs`). Cross-cutting integration tests live under each crate's top-level `tests/`. Runner is nextest (`cargo t` alias in `.cargo/config.toml`); `cargo test --workspace` works too (no nextest required).

## Module map (after reorg R1–R4)

`difflore-core` top-level modules (one module, one name — full old→new in `RENAMES.md`):

| Module | Contents |
| --- | --- |
| `contract` | Cloud API contract layer. `contract::generated` is `generate_types!` output from the vendored spec; `contract::dto` is hand-written DTOs for endpoints outside the spec. Both re-exported flat as `contract::TypeName`. |
| `context` | Retrieval pipeline: embeddings, ANN index (`index_db`), rule rendering, orchestrator. |
| `domain` | Pure domain types (models, origins, providers, rule views). Leaf layer — must not import `cloud`/`infra`/`context`/store layers (enforced by `layer-gate.sh`). |
| `error` | Crate-level `CoreError` / `Result` (moved out of `domain`). |
| `infra` | Config, crypto, db, env, files, git, paths, settings, startup. |
| `ingest` | `ingest::agent_files` (formerly `sources`), `ingest::github` (formerly `github_import`). |
| `review_engine` | Review generation (formerly `review`). |
| `review_store` | Persisted reviews (formerly `reviews`). |
| `mcp_server` | JSON-RPC 2.0 MCP server over stdin/stdout. Tests in `mcp_server/tests/`. |
| `observability` | Activity/cost/outcome telemetry + `observability::trajectory` (formerly `review_trajectory`) and `observability::classifier` (formerly `observation`). |
| `skills` | Skill filesystem (`skills::fs`, formerly `skill_fs`). |
| `team` | Team rule publish / cloud-id mapping. Tests in `team/tests.rs`. |
| `cloud` | Cloud client + outbox sync. |
| `packs` | Rule packs. |
| `migration` | Live startup guard for retired local index layouts — see "Known unwired / non-obvious" below. |

`difflore-cli` notable modules: `cli`, `commands`, `clients`, `hook`, `installer`, `runtime`, `agent_exec`, `session_mine`, `style`, `post_install_scan`.

## Hook module (R2 收口)

`difflore-cli/src/hook/` is the single home for everything between an AI
client's lifecycle hook firing and DiffLore's response. After R2 it is one
collapsed module tree:

| Submodule | Role |
| --- | --- |
| `hook/adapters/` | Per-client JSON dialects normalised into one `HookEvent` (Claude Code, Cursor, Gemini, Windsurf). |
| `hook/runtime/` | Event dispatch: rule injection, observation capture, fire logging, session-mine triggering. |
| `hook/banner/` | The since-last-session recap surfaced on `SessionStart`. |
| `hook/cache.rs` | Short-window dedup so repeated edits stay off the hot path. |
| `hook/forward/` | Local-socket forwarder + the wire protocol. |

`hook/forward/protocol.rs` is the **single line-protocol definition** both the
`difflore` binary and the `difflore-hook` shim binary compile against (wire
shapes, endpoint resolution, blocking transport). Do not fork it.

## Cloud contract pipeline (blueprint section 5)

The cloud repo (`difflore-cloud`) is the source of truth. `pnpm contract:export`
instantiates the OpenAPI generator offline and writes the full `/api` spec to
`difflore-cloud/src/contracts/openapi/api.json`, committed there.

The CLI **vendors** a copy so `openapi_contract::generate_types!` can read it at
compile time:

- `crates/difflore-core/contracts/openapi-spec.json` — the vendored spec.
- `crates/difflore-core/contracts/SOURCE` — provenance: cloud `source-commit`,
  `source-path`, and the `spec-sha256` of the vendored copy.

Type tracks are kept separate on purpose:

- `contract/generated.rs` — nothing but `generate_types!("contracts/openapi-spec.json")`.
- `contract/dto.rs` — hand-written DTOs for endpoints not yet migrated to the
  generated track. Its doc-comment header is a **registry** that may only
  shrink. Two tests enforce the anti-double-tracking invariants:
  - `dto_registry_paths_not_overlapping_spec` — every registered `METHOD /path`
    that is present in the spec must carry the literal `(in spec` marker (an
    unmarked in-spec row = undocumented generated/hand-written overlap → red).
  - `hand_written_dto_names_disjoint_from_spec_schema_names` — no hand-written
    DTO type name may collide with a generated component-schema name.

### Syncing the contract

```sh
scripts/sync-contract.sh             # sync from sibling difflore-cloud checkout
scripts/sync-contract.sh --check     # CI gate: vendored sha256 == SOURCE sha256
scripts/sync-contract.sh --cloud-repo <path>   # override cloud checkout location
```

`sync-contract.sh` decides automatically:

- **Direct adoption** — only when the cloud spec is structurally compatible
  (same top-level keys, path set, schema-name set) AND adopting it would not
  shrink the generated types. Copies the spec + refreshes SOURCE.
- **Verify-and-register (downgrade)** — when the cloud spec diverges in a way
  that would change/shrink `generate_types!` output. The vendored spec is
  **not** replaced (that would break consumers); instead the vendored sha256 is
  re-verified against SOURCE and the divergent cloud commit is registered with a
  note. Current state: the cloud HEAD spec and the vendored spec are
  structurally identical but differ by one optional field
  (`nextStep` on `/impact/flywheel-proof`'s inline response), so SOURCE carries
  a DIVERGENCE note and the spec is intentionally not re-vendored. Convergence
  is tracked for the cloud C1/C5 batches (export-diff-empty gate).

## Plugin distribution layout

Three manifests exist on purpose — they serve different install flows, **do not "dedupe" them**:

| Path | Consumer |
| --- | --- |
| `.claude-plugin/plugin.json` | Claude Code, repo added directly as a plugin |
| `.claude-plugin/marketplace.json` | Claude Code marketplace index; points at `./plugin` |
| `plugin/` (+ its `.claude-plugin/plugin.json`) | The actual plugin bundle (hooks, skills, `.mcp.json`) installed via the marketplace |
| `.codex-plugin/plugin.json` | Codex variant (intentionally different description + `interface` block) |

The two Claude manifests (root and `plugin/`) must stay **identical**; `difflore dist verify`
enforces this (`check_manifest_consistency` in `commands/dist.rs`) along with
name/version/license checks against the CLI crate version.

## Moving-files must-check list (landmines)

Moving or renaming a file can break compile-time path references that the type
system cannot catch. Before relocating any of these, update the reference and
re-run the relevant gate:

1. **`include_str!` paths (5 in `cli/tests.rs`)** — embed source of `README.md`,
   `commands/cloud/mod.rs`, `commands/doctor/report/env_probes.rs`, and the TUI
   `widgets/status_bar.rs` + `modals/fix_runs_low.rs`. Relative to the file, so
   moving the test file *or* any referenced file breaks the build. (Also one in
   `mcp_server/tests/remember_tool_tests.rs` → crate-level
   `tests/fixtures/rag-eval-seed-cases.json`, and one in `contract/dto.rs` →
   `contracts/openapi-spec.json`.)
2. **`commands/dist.rs` hard-coded skills list** — `dist verify` checks a fixed
   list of `plugin/skills/*/SKILL.md` paths. Adding/removing/renaming a skill
   requires editing that list. Always run `cargo run -p difflore-cli -- dist verify`
   after touching anything under `plugin/`.
3. **Plugin double-write** — the root and `plugin/` Claude manifests must stay
   byte-identical; edit both, then `dist verify`.
4. **`layer-gate.sh`** — every first-level dir under `crates/*/src` must be a
   declared module, and `domain/` stays pure. Adding a directory module or a
   cross-layer import in `domain/` trips the gate.

## Vocabulary (rule / skill / memory / agent)

- **rule** — a single reviewable guideline (origin: conversation, import,
  manual). Stored, rendered, and recalled by the context pipeline.
- **memory** — the persisted corpus of rules + past verdicts the CLI recalls
  from; "review memory" is the recall surface (`difflore recall` / `ask`).
- **skill** — a packaged Claude Code/Codex capability under `plugin/skills/`
  (e.g. `rule-search`, `knowledge-agent`); shipped in the plugin bundle and
  pinned in `dist.rs`.
- **agent** — an installed AI coding client wired to DiffLore via hooks + MCP
  (`difflore agents install/status/update/uninstall`).

## Known unwired / non-obvious

- **`difflore-core::migration`** — the *migration* logic is retired, but
  `migration::run_if_needed()` is a **live** fail-closed startup guard called
  from `difflore-cli/src/lib.rs` on every run and covered by
  `tests/migration_test.rs`. It refuses to proceed if a stale global
  `~/.difflore/context-index.db` exists. Do not delete it.
- **`hook::forward::try_forward` / `run_server`** — currently have **no callers**
  in the workspace. The daemon that would host `run_server`, and the client path
  that would call `try_forward`, are not yet wired. Only
  `forward::protocol::ipc_roundtrip_blocking` is live today (used by the
  `difflore-hook` shim). Both entry points are kept `pub` ahead of the daemon
  landing; wiring is deferred to a later batch.

## Verification

```sh
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings
cargo test --workspace                                  # or: cargo t (nextest)
cargo run -p difflore-cli -- dist verify                # plugin-manifest guardrail
bash scripts/layer-gate.sh                              # structural lints
bash scripts/sync-contract.sh --check                   # vendored-spec sha256 gate
```
