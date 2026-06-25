# difflore

[![CI](https://img.shields.io/github/actions/workflow/status/difflore/difflore-cli/ci.yml?branch=main&label=CI)](https://github.com/difflore/difflore-cli/actions)
[![Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![MCP](https://img.shields.io/badge/MCP-stdio-green.svg)](https://modelcontextprotocol.io)

DiffLore is an open-source CLI that turns past PR/MR review feedback into
source-backed local rules for AI coding agents.

It imports review history, stores rules in local SQLite, and serves relevant
rules through MCP, hooks, and CLI commands before an agent edits code.

## Install

macOS/Linux:

```bash
curl -fsSL https://difflore.dev/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://difflore.dev/install.ps1 | iex
```

Update later:

```bash
difflore update
```

GitHub import needs `git` and `gh auth login`. GitLab import uses a stored PAT:

```bash
echo "<TOKEN>" | difflore auth gitlab
```

## Quickstart

Try the demo:

```bash
difflore try
```

Use it in a repo:

```bash
cd your-repo
difflore init
difflore import-reviews --dry-run
difflore import-reviews
difflore agents install
difflore status
difflore recall --diff
```

DiffLore never commits, pushes, opens PRs, or posts GitHub comments.

## Commands

| Command | Purpose |
| --- | --- |
| `difflore try` | Run the demo |
| `difflore init` | Set up the current repo |
| `difflore import-reviews` | Import GitHub PR or GitLab MR review history |
| `difflore agents install` | Wire DiffLore into local agents |
| `difflore status` | Show readiness and the next command |
| `difflore memory` | Inspect local rules, drafts, queues, and autopilot state |
| `difflore memory review` | Review pending local memory |
| `difflore recall --diff` | Retrieve matching rules for the current diff |
| `difflore review --diff all` | Review the current diff without modifying files |
| `difflore fix` | Apply rule-aware local fixes |
| `difflore capabilities --json` | Print the machine-readable CLI/MCP contract |

Run `difflore --help` for the full command list.

## Agents

`difflore agents install` configures MCP and hooks for supported local agents.
Run `difflore agents status` to inspect the current machine.

Local AI CLI calls that need an LLM backend use `gate4agent`.

## Data

DiffLore is local-first:

- Rules and activity are stored in local SQLite by default.
- Cloud sync is optional and explicit through `difflore cloud ...`.
- MCP is for agent context, retrieval, explanation, and proposals.
- Approval, rejection, sync, auth, provider setup, and file mutation stay in the CLI.

## Development

```bash
cargo fmt --all --check
cargo check -p difflore-cli
cargo test -p difflore-cli
```

Issues and PRs are welcome. Do not include secrets, private PR text, or private
code in examples.

For suspected vulnerabilities, email **hello@difflore.dev** instead of opening
a public issue. See [SECURITY.md](SECURITY.md).

## License

Apache 2.0. See [LICENSE](LICENSE).
