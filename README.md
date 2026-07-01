# difflore

[![CI](https://img.shields.io/github/actions/workflow/status/difflore/difflore-cli/ci.yml?branch=main&label=CI)](https://github.com/difflore/difflore-cli/actions)
[![Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![MCP](https://img.shields.io/badge/MCP-stdio-green.svg)](https://modelcontextprotocol.io)

DiffLore turns your team's past PR/MR review comments into source-backed rules
your local AI coding agents can recall before they write code.

The open-source CLI runs locally: import private GitHub or GitLab review
history, approve the useful rules, and serve the relevant ones to agents through
MCP and hooks. DiffLore never commits, pushes, opens PRs, or posts GitHub
comments.

Hosted DiffLore Cloud is optional. The local CLI does not have a hosted PR quota;
cloud quotas apply only to managed GitHub App / team workflows.

## Why

AI coding agents know public code. They do not know the review decisions your
team already made:

- "Use this helper in billing paths."
- "Do not bypass this auth wrapper."
- "This package handles retries; do not add a second loop."
- "This service rejects raw SQL outside migrations."

Those rules are usually buried in old review threads. DiffLore mines them into
local memory, keeps source evidence attached, and gives agents the relevant
rules before they edit the files that trigger them.

## Install

```bash
curl -fsSL https://difflore.dev/install.sh | sh
```

Update later:

```bash
difflore update
```

GitHub import uses your local `git` remote and GitHub CLI auth:

```bash
gh auth login
```

GitLab import uses a stored PAT with `read_api` scope:

```bash
echo "<TOKEN>" | difflore auth gitlab
```

## Quickstart

Try the demo:

```bash
difflore try
```

Use it in a real repo:

```bash
cd your-repo
difflore init
difflore import-reviews --dry-run
difflore import-reviews
difflore memory
difflore memory review
difflore agents install
difflore recall --diff
difflore review --diff all
```

That flow imports merged review history, turns review comments into local memory
candidates, lets you approve or reject them, and wires DiffLore into detected
local agents.

## Local by default

DiffLore works with private repos and local AI CLIs. A cloud account is not
required for the local workflow.

- Local rules and activity are stored in local SQLite.
- `difflore import-reviews` writes locally unless you explicitly pass `--upload`.
- `difflore cloud login` and `difflore cloud sync` are opt-in.
- Raw local queues are not uploaded by default; cloud sync requires explicit
  flags for observations, candidates, or telemetry.
- Static exports to `AGENTS.md` / `CLAUDE.md` are optional snapshots. Live
  `agents install` is the preferred path because it is diff-aware.

## Agent support

Run:

```bash
difflore agents install
difflore agents status
```

DiffLore installs MCP entries and lifecycle hooks where the local agent supports
them. The installer detects common local coding agents, including Claude Code,
Codex, Cursor, Gemini CLI, Windsurf, Goose, OpenCode, Copilot CLI, Crush, Roo
Code, Warp, and Antigravity.

## Commands

| Command | Purpose |
| --- | --- |
| `difflore try` | Run the demo |
| `difflore init` | Set up the current repo |
| `difflore import-reviews` | Import private GitHub PR or GitLab MR review backlog locally |
| `difflore memory` | Show remembered rules, review queue, paused rules, sync state, and next action |
| `difflore memory review` | Review pending local memory |
| `difflore agents install` | Wire DiffLore into local AI CLIs and agents |
| `difflore agents status` | Show which agents are connected |
| `difflore status` | Show readiness and the next command |
| `difflore recall --diff` | Retrieve matching rules for the current diff |
| `difflore review --diff all` | Review the current diff without modifying files |
| `difflore fix` | Apply rule-aware local fixes |
| `difflore ask "..."` | Ask the team's source-backed rules a question |
| `difflore export` | Write a static snapshot to `AGENTS.md` or `CLAUDE.md` |
| `difflore capabilities --json` | Print the machine-readable CLI/MCP contract |

Run `difflore --help` for the full command list.

## Optional Cloud

The cloud layer is for teams that want hosted GitHub App ingestion, shared team
rules, dashboards, managed semantic recall, governance, and audit workflows.

```bash
difflore cloud login
difflore cloud status
difflore cloud sync
difflore memory team-candidates
```

Use the local CLI first when you want a no-account path. Use cloud when multiple
people need one shared memory and review workflow.

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
