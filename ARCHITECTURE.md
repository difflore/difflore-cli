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
- Tests: unit tests in `#[cfg(test)]` modules, integration tests under each crate's `tests/`. Runner is nextest (`cargo t` alias in `.cargo/config.toml`).

## Cloud contract

`crates/difflore-core/openapi-spec.json` is the single source for cloud API types —
`cloud/api_types.rs` generates from it via `openapi_contract::generate_types!`.
It is maintained by hand; sync it when the cloud (difflore-cloud) API changes.

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

## Verification

```sh
cargo t                       # nextest, falls back to: cargo test
cargo clippy --all-targets
cargo run -p difflore-cli -- dist verify   # release guardrail for plugin manifests
```
