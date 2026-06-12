# difflore

[![CI](https://img.shields.io/github/actions/workflow/status/difflore/difflore-cli/ci.yml?branch=main&label=CI)](https://github.com/difflore/difflore-cli/actions)
[![Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![MCP](https://img.shields.io/badge/MCP-stdio-green.svg)](https://modelcontextprotocol.io)

Your AI coding agent learned public code, not the decisions your team made in
private PR reviews. DiffLore is an open-source CLI that turns those reviews
into source-backed codebase rules for local AI agents.

It imports review feedback your team already wrote, stores the resulting rules
in local SQLite, and serves the relevant ones to Claude, Codex, Cursor, and
other agents through MCP, installed hooks, or the CLI before they edit.

## Runtime ROI

DiffLore reports value from the actual local loop, not a canned benchmark:

| Before DiffLore | After DiffLore |
|---|---|
| Same review comments repeat across PRs | Repeated comments become local rules agents can recall |
| Review memory is hidden in GitHub history | `difflore recall --diff` shows matching memories before edits land |
| ROI requires a dashboard hunt | `difflore status`, `recall --diff`, and accepted `fix` runs show `~N review-minutes saved` and recall counts |

Example runtime receipt:

```text
Value (last 30d): ~12 review-minutes saved | 8 recalls | 3 ready for agents
```

## Pick Your Path

| Path | Best for | Install | Account needed | Sync |
|---|---|---|---|---|
| Local CLI | Individual developers proving value in one repo | one `difflore` binary + `difflore agents install` | No | Local SQLite only |
| Team Cloud | Teams sharing review memory across people and machines | Local CLI plus `difflore cloud login` | Yes | Team rules, managed embeddings, dashboards |

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/difflore/difflore-cli/releases/latest/download/difflore-cli-installer.sh | sh
```

Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/difflore/difflore-cli/releases/latest/download/difflore-cli-installer.ps1 | iex"
```

Other install paths:

```bash
brew install difflore/tap/difflore
cargo install difflore-cli
cargo install --git https://github.com/difflore/difflore-cli difflore-cli # unreleased main
```

Prerequisites for importing GitHub PR reviews: `git` and GitHub CLI `gh`.
Run `gh auth login` once before importing PR reviews. GitLab repos need no
extra CLI — store a personal access token once with `difflore auth gitlab`
(see [Importing from GitLab](#importing-from-gitlab)).

## Quickstart

Run the bundled demo without touching a repo:

```bash
difflore try
```

Use it in a GitHub or GitLab repo:

```bash
cd your-repo
difflore init
difflore import-reviews --dry-run
difflore import-reviews
difflore recall --diff
difflore agents install
```

You can also start before importing history: once agents are wired, tell your
agent "remember this" for a review rule you want to keep. Conversation captures
land locally immediately; cloud sync and review import are upgrades, not a
prerequisite.

After setup, your agent can ask DiffLore for source-backed codebase rules
before it edits a file. You can also preview or apply rule-aware local fixes:

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
| `difflore import-reviews` | Import GitHub PR / GitLab MR review history |
| `difflore recall --diff` | Preview relevant rules for the current diff |
| `difflore fix --preview` | Preview rule-aware local fixes |
| `difflore status` | Show local memory health and next steps |
| `difflore agents install` | Wire supported local agents |
| `difflore export` | Write team rules into `AGENTS.md` / `CLAUDE.md` (static snapshot) |
| `difflore update` | Refresh agent blocks, hook shims, and doctor checks |
| `difflore doctor --report` | Write a diagnostic report |

Run `difflore --help` for the full command list.

## Importing from GitLab

`difflore import-reviews` also imports merge request discussions from GitLab —
both **gitlab.com and self-managed instances** (subgroups included). It talks
to the GitLab REST API directly over HTTPS with a personal access token, so it
passes enterprise IT policy without installing `glab` or any other CLI.

One-time setup — mint a PAT with the **`read_api` scope only** (no write
access is ever needed) and store it encrypted:

```bash
echo "<TOKEN>" | difflore auth gitlab                  # gitlab.com
echo "<TOKEN>" | difflore auth gitlab --host gitlab.corp.example
difflore auth gitlab --check                           # verify before importing
```

A PAT is required even for public projects: gitlab.com rejects anonymous calls
to the MR discussions API.

Then import as usual — `gitlab.com` remotes are detected automatically, and a
self-managed host is detected automatically once its PAT is stored:

```bash
cd your-repo
difflore import-reviews --dry-run
difflore import-reviews
```

Without a stored PAT, point a one-off run at a self-managed instance
explicitly (`--gitlab-host` implies the GitLab provider):

```bash
difflore import-reviews --gitlab-host gitlab.corp.example --repo group/subgroup/project
```

GitLab specifics worth knowing:

- `--pr <N>` means the MR IID (the `!N` number); `--max-prs` caps MRs.
- A `404` from GitLab can mean a wrong project path **or** missing token
  access — GitLab deliberately answers 404 (not 403) for private projects
  your token cannot see. The error text walks you through both.
- Self-managed instances behind a private CA need that CA trusted at the OS
  level; DiffLore uses the platform certificate verifier and has no
  insecure-skip option.
- v1 imports merged MRs only; `--from-upstream` and `--include-open` are
  GitHub-only for now.

See [docs/cli-reference.md](docs/cli-reference.md) for the full provider
resolution rules and token handling.

## How Injection Works

Hook-based injection is engineered to be useful without becoming noise:

- **~1,500-token hard budget.** Each hook injection is capped at roughly
  1,500 tokens (a single oversized rule may exceed the cap rather than inject
  nothing, hence the "~").
- **Right moment, no spam.** Rules are injected after each edit, deduplicated
  per file for 120 seconds, and the hook goes quiet automatically when recall
  comes back empty.
- **Strict per-repo isolation.** Rules are recalled only for the repo they
  were learned from (matched against your git remotes); there is no cross-repo
  or global fallback.
- **Every injection is accounted for.** Each serve is logged locally with its
  estimated token cost, and when an agent actually cites a rule that citation
  is telemetered back — so reported value reflects real use, not volume.

## Export to Static Context Files

Some agents and teammates only read static context files. `difflore export`
writes the rules agents would recall in this repo into a marker-delimited
section of `AGENTS.md` and/or `CLAUDE.md` at the repo root:

```bash
difflore export                          # AGENTS.md + CLAUDE.md
difflore export --format claude-md --dry-run
difflore export --local-only             # exclude team/cloud-synced rules
```

Only the section between the `BEGIN/END DIFFLORE RULES` markers is managed;
your content around it is never touched. Commit the exported files to share
them, or gitignore them yourself — DiffLore never edits `.gitignore`.

The export is a point-in-time snapshot: it goes stale as rules evolve and it
cannot match rules to the file being edited. Prefer `difflore agents install`
(MCP + hooks) for live, diff-aware injection.

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

Merging to `main` does not publish a release. Maintainers publish from a
release commit by bumping crate versions and `CHANGELOG.md`, then pushing a
`vX.Y.Z` tag.

On a release tag, GitHub Actions builds GitHub Release artifacts and publishes
the Homebrew formula. The independent `Publish crates` workflow publishes
crates.io packages in dependency order, or skips versions that are already
published. For the first crates.io release, publish the crates manually once or
temporarily add `CARGO_REGISTRY_TOKEN`; after that, use crates.io Trusted
Publishing and remove long-lived tokens.

Issues and PRs are welcome. Do not include secrets, private PR text, or
private code in examples.

For suspected vulnerabilities, email **hello@difflore.dev** instead of opening
a public issue.

## License

Apache 2.0. See [LICENSE](LICENSE).
