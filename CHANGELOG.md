# Changelog

Notable DiffLore changes are listed here. The project follows Semantic
Versioning.

## [Unreleased]

## [0.3.0] - 2026-06-26

### Added

- Re-enabled managed binary self-update for official installer installs.

## [0.2.0] - 2026-06-24

First general release. DiffLore turns past code-review feedback into local
memory that your AI coding agent recalls automatically.

### Added

- Import review history from GitHub and GitLab pull/merge requests.
- Automatic recall of relevant past reviews, delivered to agents over MCP and
  lifecycle hooks.
- `difflore memory` — a local control plane to inspect, curate, and prune what
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
