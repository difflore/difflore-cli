# difflore

[![CI](https://img.shields.io/github/actions/workflow/status/difflore/difflore-cli/ci.yml?branch=main&label=CI)](https://github.com/difflore/difflore-cli/actions)
[![Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![MCP](https://img.shields.io/badge/MCP-stdio-green.svg)](https://modelcontextprotocol.io)

DiffLore is an open-source CLI that turns past GitHub PR review comments into
local memory for AI coding agents.

It imports review feedback your team already wrote, stores the resulting rules
in local SQLite, and serves the relevant ones to agents through MCP, installed
hooks, or the CLI.

```bash
cargo install --git https://github.com/difflore/difflore-cli difflore-cli
```

DiffLore is not published on crates.io yet. Use the GitHub install path above
until the `difflore-cli` crate is released.

Prerequisites: `cargo` with Rust 1.87+, `git`, and GitHub CLI `gh`.
Run `gh auth login` once before importing PR reviews.

## Quickstart

Run the bundled demo without touching a repo:

```bash
difflore try
```

Use it in a GitHub repo:

```bash
cd your-repo
difflore init
difflore import-reviews --dry-run
difflore import-reviews
difflore recall --diff
difflore agents install
```

After setup, your agent can ask DiffLore for review memory before it edits a
file. You can also preview or apply rule-aware local fixes:

```bash
difflore fix --preview
```

DiffLore never commits, pushes, opens PRs, or posts GitHub comments.

<p align="center"><img src="assets/demo.svg" alt="DiffLore terminal demo" /></p>

## Common Commands

| Command | Purpose |
|---|---|
| `difflore try` | Run the zero-setup demo |
| `difflore init` | Set up DiffLore for the current repo |
| `difflore import-reviews` | Import GitHub PR review history |
| `difflore recall --diff` | Preview memories for the current diff |
| `difflore fix --preview` | Preview rule-aware local fixes |
| `difflore status` | Show local memory health and next steps |
| `difflore agents install` | Wire supported local agents |
| `difflore doctor --report` | Write a diagnostic report |

Run `difflore --help` for the full command list.

## Local First

The default path is one Rust binary plus local SQLite. No cloud account is
needed.

Data leaves your laptop only when you opt in. The local CLI does not require a
cloud account.

- `difflore cloud ...` commands are for optional team workflows.
- `difflore embeddings setup` can use your own OpenAI-compatible embedding key
  for semantic recall.
- `difflore import-reviews --upload` uploads imported review data instead of
  keeping the import local.

If cloud or embeddings are unavailable, local keyword and file-pattern recall
still works.

## Supported Agents

`difflore agents install` can wire DiffLore into supported local agents such as
Claude Code, Cursor, Gemini CLI, Windsurf, and MCP-capable CLIs. Run
`difflore agents status` for the current list on your machine.

## Development

```bash
cargo fmt --all --check
cargo check -p difflore-cli
cargo test -p difflore-cli
```

Issues and PRs are welcome. Do not include secrets, private PR text, or
private code in examples.

For suspected vulnerabilities, email **hello@difflore.dev** instead of opening
a public issue.

## License

Apache 2.0. See [LICENSE](LICENSE).
