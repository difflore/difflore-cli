# DiffLore CLI Reference

Command-level reference for surfaces that need more depth than `--help`
provides. Incremental: chapters are added as commands grow non-obvious
behavior. For the full command list run `difflore --help`.

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
