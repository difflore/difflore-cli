# Memory Governance UX Implementation Plan

> **For agentic workers:** REQUIRED: Use `superpowers:subagent-driven-development` or `superpowers:executing-plans` to implement this plan. Keep edits scoped to the files assigned to your chunk. Mark checkboxes as work completes.

**Goal:** Turn DiffLore memory from an internal rule/candidate/outbox listing into a product surface that clearly answers: what is remembered, what needs review, what is paused, whether sync is healthy, and whether the extracted memories are useful.

**Architecture:** Add a product-facing memory overview layer above the existing `MemoryInbox`, autopilot digest, activity, and cloud APIs. Preserve low-level lifecycle detail for JSON/debug flows, but make default CLI output user-governable and value-oriented.

**Tech Stack:** Rust workspace, `sqlx` SQLite, serde JSON contracts, existing `difflore memory` commands, existing cloud OpenAPI client helpers.

---

## Current Findings

- Local `memory disable` currently changes active rules back to `pending`, which makes disabled memories look like drafts that need review.
- `difflore memory` default output is closer to an implementation inventory than a product overview.
- Active memory, pending local drafts, autopilot candidates, session candidates pending upload, and cloud team candidates are represented by different models and commands.
- Cloud already exposes rule candidate routes in OpenAPI, but the CLI/core client does not yet expose them as first-class product actions.
- The product should keep advanced rules/config available, but the primary user-facing language should be: remembered, needs review, paused, sync, useful/not useful.

---

## Work Chunks

### Chunk 1: Fix Disabled Rule Semantics

**Owner:** Worker A

**Files:**

- `crates/difflore-core/src/memory_autopilot/log.rs`
- `crates/difflore-core/src/memory_autopilot/mod.rs`
- `crates/difflore-core/src/memory_inbox.rs` only if necessary

**Tasks:**

- [ ] Add a failing test showing `disable_rule` moves an active rule to `disabled`, not `pending`.
- [ ] Add/assert behavior that disabled rules do not appear in local drafts/candidates.
- [ ] Update `disable_rule` SQL and event payload from `active -> pending` to `active -> disabled`.
- [ ] Confirm existing pending draft behavior remains unchanged.

**Verification:**

- [ ] `rtk cargo test -p difflore-core memory_autopilot:: --lib`
- [ ] `rtk cargo test -p difflore-core memory_inbox --lib`

### Chunk 2: Add Core Memory Overview Model

**Owner:** Worker B

**Files:**

- `crates/difflore-core/src/memory_overview.rs`
- `crates/difflore-core/src/lib.rs`

**Tasks:**

- [ ] Create `MemoryOverviewOptions` with repo scope, latest item limit, and activity window days.
- [ ] Create serializable overview structs for remembered, needs review, paused, sync, activity, next action, and debug details.
- [ ] Load active memories via existing memory item APIs and count both global and repo-scoped active rules.
- [ ] Load local drafts/discoveries via `load_memory_inbox`.
- [ ] Load autopilot review groups via existing digest APIs.
- [ ] Load pending upload counts for approved session candidates and activity records.
- [ ] Load activity metrics for the requested window: recall calls, empty recalls, surfaced rules.
- [ ] Export the module from `difflore-core`.

**Target Shape:**

```rust
pub struct MemoryOverview {
    pub schema_version: String,
    pub remembered: RememberedOverview,
    pub needs_review: NeedsReviewOverview,
    pub paused: PausedOverview,
    pub sync: SyncOverview,
    pub activity: ActivityOverview,
    pub next: MemoryOverviewNextAction,
    pub debug: MemoryOverviewDebug,
}
```

**Verification:**

- [ ] `rtk cargo test -p difflore-core memory_overview --lib`
- [ ] `rtk cargo check -p difflore-core`

### Chunk 3: Add Cloud Candidate Client

**Owner:** Worker C

**Files:**

- `crates/difflore-core/src/cloud/candidates.rs`
- `crates/difflore-core/src/cloud/mod.rs`

**Tasks:**

- [ ] Add DTOs for team rule candidates from the current OpenAPI contract.
- [ ] Add client helpers for listing and counting team candidates.
- [ ] Add client helpers for approve/reject candidate actions if the API already supports them.
- [ ] Add helper coverage for settings/dismiss routes only if they are present in OpenAPI and fit existing cloud module style.
- [ ] Keep auth and error handling consistent with existing cloud helpers.

**Verification:**

- [ ] `rtk cargo test -p difflore-core cloud::candidates --lib`
- [ ] `rtk cargo check -p difflore-core`

### Chunk 4: Integrate Product Overview Into CLI

**Owner:** Main agent after Chunks 1-3

**Files:**

- `crates/difflore-cli/src/commands/memory/overview.rs`
- `crates/difflore-cli/src/commands/memory/mod.rs`
- `crates/difflore-cli/src/commands/memory/inbox.rs`
- CLI argument/dispatch files only if needed

**Tasks:**

- [ ] Make default `difflore memory` render the product overview.
- [ ] Preserve existing JSON output shape only where compatibility requires it; otherwise emit the new overview under a stable schema version.
- [ ] Human output sections should be remembered, needs review, paused, sync, activity, and next action.
- [ ] Hide raw internal terms like outbox, rule record, digest group, and session candidate unless the user asks for debug/JSON.
- [ ] Keep existing `inbox`, `active`, `activity`, `digest`, `recommended`, `conflicts`, approve/reject/disable commands working.

**Verification:**

- [ ] `rtk cargo run -p difflore -- memory`
- [ ] `rtk cargo run -p difflore -- memory --json`
- [ ] `rtk cargo run -p difflore -- memory inbox`

### Chunk 5: Add Team Candidate UX

**Owner:** Main agent after Chunk 3

**Files:**

- `crates/difflore-cli/src/cli/commands.rs`
- `crates/difflore-cli/src/dispatch.rs`
- `crates/difflore-cli/src/commands/memory/*.rs`

**Tasks:**

- [ ] Add a clear CLI surface for cloud/team candidates if the API client is complete.
- [ ] Commands should support list/count and approve/reject where API support exists.
- [ ] Human output should frame them as "team suggestions" or "needs review", not implementation lifecycle records.
- [ ] JSON output should include IDs and enough metadata for automation.

**Verification:**

- [ ] `rtk cargo run -p difflore -- memory team-candidates --help`
- [ ] `rtk cargo run -p difflore -- memory team-candidates --json`

### Chunk 6: Final Verification And Review

**Owner:** Main agent

**Tasks:**

- [ ] Review all worker diffs for conflicts and product consistency.
- [ ] Run focused tests for changed core modules.
- [ ] Run CLI checks/build.
- [ ] Run a debug build if the user still wants the binary copied/built into the user directory.
- [ ] Summarize remaining product choices separately from implemented behavior.

**Verification:**

- [ ] `rtk cargo test -p difflore-core memory_autopilot:: --lib`
- [ ] `rtk cargo test -p difflore-core memory_inbox --lib`
- [ ] `rtk cargo test -p difflore-core memory_overview --lib`
- [ ] `rtk cargo check -p difflore-core`
- [ ] `rtk cargo check -p difflore`
- [ ] `rtk cargo build -p difflore`

---

## Product Principles

- The default surface should help a user decide, not expose every internal lifecycle state.
- Memory should feel governable: users can see what agents use, what is waiting, what is paused, and what will happen next.
- Advanced rule machinery should remain available for power users and automation, mostly via subcommands and JSON.
- Candidate automation should be framed as a review workflow with clear thresholds and reversibility.
- The product should show value signals before adding more knobs: recall usage, empty recall rate, surfaced rules, and recent accepted memories.
