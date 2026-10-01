# Changelog

Notable DiffLore changes are listed here. The project follows Semantic
Versioning.

## [Unreleased]

## [0.8.0] - Unreleased

difflore is now one command: point it at a GitHub repo and it prints the
unwritten rules from that repo's code review history.

### Changed

- `difflore <owner/repo>` reads recent maintainer review comments through the
  GitHub API, asks your own agent CLI (Claude Code, Codex or Pi) which
  expectations recur, and prints each rule with a link to a review it came
  from. `--write` saves them to AGENTS.md, CLAUDE.md or a Cursor rule.
- The workspace is a single crate with five dependencies.
- Installs through the shell, PowerShell and npm installers.

### Removed

- Cloud sync, accounts and billing, including the upload of edited file bodies
  after `cloud login`.
- GitLab import, the MCP server, agent hooks, rule recall, `review`, `fix`,
  `ask`, embeddings, session mining, the Claude Code and Codex plugins, and the
  GitHub Action.

## [0.7.0] - 2026-07-06

### Added

- Added source-kind provenance and author-type ingest for review-derived rule
  evidence.
- Added bot activation gating for cloud-aware recall and sync flows.

### Changed

- Updated the vendored Cloud OpenAPI contract for team capacity, impact
  drilldowns, and rule recall evidence.
- Improved recall union coverage so accepted fixes can match prior cloud and
  local rule evidence more consistently.

## [0.6.0] - 2026-07-01

### Added

- Added value telemetry to the `--json` payloads of `status`, `doctor`, cloud
  sync, and cloud team commands.
- Added repo alias storage so local proof can be matched to GitHub repositories
  more reliably.
- Added redacted proof summary sync for the cloud dashboard.

### Changed

- Updated the vendored Cloud OpenAPI contract used by the CLI.
- Improved rules triage and recall proof reporting for local dogfood data.

## [0.5.0] - 2026-06-30

### Added

- Added `difflore rules team-candidates` for reviewing, counting, showing,
  approving, and rejecting team rule suggestions.
- Added a richer `difflore rules summary` overview covering active rules,
  review queues, paused rules, sync state, and recent recall activity.
- Added cloud candidate client support for team rule governance workflows.

### Changed

- Disabling a rule now pauses it as `disabled` instead of moving it back
  to pending review.
- Disabled rules are excluded from recall and local review queues.

### Removed

- Removed the bundled `pre-submit-review` MCP skill.

## [0.4.0] - 2026-06-29

### Added

- Added local-agent review import distillation for extracting higher-quality
  rule candidates from GitHub and GitLab review history.
- Added richer MCP rule evidence, safety guidance, and recall diagnostics for
  agents consuming DiffLore rules.

### Changed

- Improved review import filtering, repo/file scoping, and deterministic
  fallback behavior when local agent distillation is unavailable.
- Improved recall ranking with query-signal expansion, stricter file-pattern
  handling, and safer hook rule injection budgets.
- Updated installer and onboarding flows for more reliable MCP configuration.

### Fixed

- Fixed release preflight stability by removing obsolete ignored benchmark
  tests.

## [0.3.0] - 2026-06-26

### Added

- Re-enabled managed binary self-update for official installer installs.

## [0.2.0] - 2026-06-24

First general release. DiffLore turns past code-review feedback into local
rules that your AI coding agent recalls automatically.

### Added

- Import review history from GitHub and GitLab pull/merge requests.
- Automatic recall of relevant past reviews, delivered to agents over MCP and
  lifecycle hooks.
- `difflore rules` — a local control plane to inspect, curate, and prune what
  the agent recalls.
- Semantic embeddings on by default for higher-recall retrieval, with automatic
  full-text fallback.
- Static rule export to `AGENTS.md` and `CLAUDE.md`.
- Fix previews, `difflore doctor` diagnostics, and optional cloud sync.

### Changed

- Faster, hardened lifecycle hooks with lower cold-start latency, especially on
  Windows.
- Polished terminal output, authentication, and first-run experience.

### Removed

- The experimental terminal UI and first-run wizard.

## [0.1.0] - 2026-06-08

- Initial preview to validate the local-first approach; not publicly promoted.
