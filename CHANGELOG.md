# Changelog

All notable changes to DiffLore are listed here. The project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-06-08

### Added

- First public release of the local-first DiffLore CLI.
- GitHub PR review import with `difflore import-reviews`, including dry-run
  planning and repo-scoped rule attribution.
- Local review-memory recall with `difflore recall`, `difflore ask`, and
  installed MCP tools for supported AI coding agents.
- Agent wiring with `difflore agents install`, `agents status`, `agents update`,
  and `agents uninstall`.
- Rule-aware local fix previews with `difflore fix --preview`; accepted changes
  only touch the local working tree.
- Local status and diagnostics through bare `difflore`, `difflore status`, and
  `difflore doctor --report`.
- Optional semantic recall configuration through `difflore embeddings setup`.
- Optional cloud login and sync commands for teams that want shared memory,
  governance, and impact views.
- Public documentation for installation, CLI usage, security reporting,
  contribution workflow, and release notes.
