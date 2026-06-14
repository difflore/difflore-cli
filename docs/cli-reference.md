# DiffLore CLI Reference

Command-level reference for surfaces that need more depth than `--help`
provides. Incremental: chapters are added as commands grow non-obvious
behavior. For the full command list run `difflore --help`.

## `difflore import-reviews` — GitLab provider

Imports merged-MR discussions from gitlab.com or a self-managed GitLab
instance into the same local review store the GitHub importer feeds, so local
candidate drafting, `--upload`, and `difflore recall` work identically for
both providers.

```bash
difflore import-reviews                          # provider auto-detected from the git remote
difflore import-reviews --provider gitlab        # force GitLab (host from remote, else gitlab.com)
difflore import-reviews --gitlab-host gitlab.corp.example --repo group/sub/project
difflore import-reviews --pr 42 --pr 57          # exact MR IIDs (the !N numbers)
difflore import-reviews --dry-run --json         # plan only; payload carries provider + gitlabHost
```

### Provider resolution

Explicit flags win, then the git remote decides, and unknown hosts fail loud
instead of guessing:

1. `--gitlab-host <HOST>` → GitLab on that host (conflicts with
   `--provider github`).
2. `--provider github` / `--provider gitlab` → forced; forced GitLab adopts
   the remote's host when it plausibly is the instance (never `github.com`),
   else defaults to `gitlab.com`.
3. No flags → the `origin` remote: `github.com` → GitHub; `gitlab.com` or any
   host with a PAT stored via `difflore auth gitlab --host <HOST>` → GitLab.
4. Any other remote host → an error asking for an explicit `--provider`
   (storing a PAT for the host makes detection automatic afterwards).

`--repo` accepts the full namespace path (`group/project`,
`group/subgroup/project`). When omitted, the path is taken from the remote
only if the remote host matches the resolved GitLab host — a mismatch errors
rather than importing from the wrong instance.

### Flag semantics in the GitLab context

| Flag | GitLab meaning |
|---|---|
| `--pr <N>` | MR IID (the `!N` number), not the global MR id. Repeatable. |
| `--max-prs <N>` | Maximum MRs to import (newest-updated first). |
| `--exclude-prs <CSV>` | MR IIDs that must contribute zero rules (leak-free recall eval). |
| `--since <YYYY-MM-DD>` | Pushed server-side as `updated_after` (GitLab's MR list filters on update time, not merge time). |
| `--from-upstream` | Not supported for GitLab (errors; GitHub-only fork flow). |
| `--include-open` | Not supported for GitLab yet (errors; merged MRs only). |

### Token handling

The token must have the **`read_api` scope** (nothing more) and is resolved
in this order: `DIFFLORE_GITLAB_TOKEN` env → `GITLAB_TOKEN` env → encrypted
storage written by `difflore auth gitlab` (keyed per host, so gitlab.com and
any number of self-managed instances can coexist). A PAT is required even for
public projects — gitlab.com rejects anonymous calls to the MR discussions
API.

### Error semantics and transport

- A preflight `GET /api/v4/projects/:id` turns auth/visibility problems into
  a precise error before any MR work starts.
- **404 means path *or* permissions:** GitLab answers 404 (not 403) for
  private projects the token cannot see. Check the project path and that the
  PAT has `read_api` with at least Reporter access.
- 429/5xx responses are retried with exponential backoff (5s/10s/20s/40s),
  honoring a `Retry-After` header (capped at 120s).
- TLS uses the operating system's trust store. Self-managed instances with a
  private CA need that CA installed at the OS level; there is no
  insecure-skip option.

### What gets imported

Merged-MR discussions: note bodies, inline positions (`new_path` /
`new_line`), discussion-level resolution, and later replies in the same
thread (both feed the durability signal that routes candidates). GitLab
system notes ("changed the description", …) are filtered out. Per-note award
emoji are deliberately not fetched in v1 — that would cost one extra request
per note, and the durability signal is neutral-by-default without them.

## `difflore auth gitlab`

Stores, verifies, and removes GitLab personal access tokens (encrypted at
rest, keyed per host).

```bash
echo "<TOKEN>" | difflore auth gitlab                        # store for gitlab.com
echo "<TOKEN>" | difflore auth gitlab --host gitlab.corp.example
difflore auth gitlab --check [--host <HOST>]                 # verify against /api/v4/user
difflore auth gitlab --remove [--host <HOST>]                # delete the stored token
```

Tokens are accepted from piped stdin or the `DIFFLORE_GITLAB_TOKEN` /
`GITLAB_TOKEN` env vars — never a `--token` flag and never an interactive
prompt, both of which leak into shell history or scrollback. `--check`
reports which source (env or storage) the importer would actually use.

Storing a PAT for a self-managed host also registers that host for provider
auto-detection: subsequent `difflore import-reviews` runs in repos whose
remote points at the host pick the GitLab path without flags.

## `difflore export`

Write the rules agents would recall in this repo into static agent context
files at the repo root, inside a marker-delimited section.

```bash
difflore export                              # AGENTS.md + CLAUDE.md
difflore export --format agents-md           # one emitter
difflore export --format claude-md --format agents-md
difflore export --dry-run                    # plan only, nothing written
difflore export --json                       # machine-readable plan/result
difflore export --no-examples                # skip Bad/Good example blocks
difflore export --local-only                 # exclude team/cloud-synced rules
difflore export --max-rules 20               # cap the export to 20 rules
```

### Flags

| Flag | Effect |
|---|---|
| `--format <agents-md\|claude-md\|all>` | Target format; repeatable. Default `all`. |
| `--dry-run` | Print the export plan (`create` / `update` / `unchanged` / `skipped`) without writing. |
| `--json` | Emit the plan/result as JSON. |
| `--no-examples` | Omit Bad/Good example blocks to keep the export small. |
| `--local-only` | Export local rules only; team/cloud-synced rules are excluded. |
| `--max-rules <N>` | Cap the export to the first `N` rules of the deterministic order (name, then id). Unlimited when omitted; `N` must be ≥ 1. |

When the cap drops rules, the plan says so: the human output shows
`N of M rules (--max-rules cap)` per target, and the `--json` report carries
`truncated: true` plus `total_rules` (the in-scope count before the cap)
alongside `rules` (the count actually exported). Because the cap keeps a
prefix of the same deterministic order, re-exports with the same cap stay
byte-stable and the content-hash short-circuit still applies.

### Which rules are exported

The same project-scope rule as runtime recall, plus explicit local rules:

- rules learned from this repo — the rule's source repo must match one of the
  repo's git remotes (`origin`, then `upstream`); there is no cross-repo or
  global fallback,
- explicit local rules you created on this machine that are not attributed to
  any repo.

Per format: `claude-md` exports only rules enabled for the claude engine
(parity with what the hook/MCP path serves Claude); `agents-md` has no engine
filter because any agent may read `AGENTS.md`.

By default the export includes team/cloud-synced rules that are in scope;
`--local-only` excludes them.

### The marker block

Everything DiffLore writes lives between two markers:

```text
<!-- BEGIN DIFFLORE RULES -->
... AUTO-GENERATED header (version, generated-at, rule count, content hash, repo scope)
... rendered rules
<!-- END DIFFLORE RULES -->
```

- Content **outside** the markers is preserved byte-for-byte (including CRLF
  line endings). Re-running `export` regenerates only the section between the
  markers.
- Re-exports are idempotent: when the rendered rule content is unchanged, the
  file is not rewritten (the embedded content hash short-circuits the write).
- Writes are atomic (temp file + rename), so an interrupted export cannot
  truncate your file.
- Safety refusals: if the target file is a **symlink**, or the marker pair is
  **corrupted** (a `BEGIN` without its `END`), the export skips that file with
  a warning and exits non-zero instead of guessing.
- Hand-edits between the markers are overwritten by the next export. Put your
  own content outside the markers.

### Side effects and sharing

`difflore export` writes only the listed files in the current repo's working
tree. It never commits, pushes, opens PRs, or edits `.gitignore`. Commit the
exported files if you want teammates and CI agents to see them, or add them to
`.gitignore` yourself.

### Static snapshot vs live injection

The export is a point-in-time projection of the rule corpus. It goes stale as
rules evolve, and it cannot match rules to the file being edited. Prefer
`difflore agents install` (MCP + hooks) for live, diff-aware injection with a
token budget and per-file deduplication; use `export` for agents or teammates
that only read static context files.
