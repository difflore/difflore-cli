// Generated from `plugin/skills/**/SKILL.md` so the published `difflore-core`
// crate can embed MCP skill resources without keeping a second skill tree.
// Update by regenerating from the root plugin skill files.
#![allow(clippy::needless_raw_string_hashes)]

pub(super) const RULE_SEARCH_SKILL_MD: &str = r################"---
name: rule-search
description: Search the local DiffLore rule library before editing code, reviewing a diff, or answering a repo-convention question.
---

# Rule Search

Find the rules that apply before you edit or review.

```text
search_rules(intent="async executor boundary", file="src/worker.rs", top_k=5)
get_rules(ids=["conv-a1f9c"], file="src/worker.rs")   # only the 1-3 that apply
```

Add `rule_timeline(rule_id="<id>", depth_before=5, depth_after=5)` only when a
hit is borderline, disputed, or maybe stale.

## Avoid

- Don't fetch full bodies before searching.
- Don't use large `top_k` unless the user asked for a broad audit.
- Don't re-query every tool call — transports may cache hits.
- Don't capture or claim new learning from search results; use
  `remember-rule-guide` or `rule-gap` when the user wants a rule saved.

## Related

`remember-rule-guide` — save a rule · `rule-gap` — find missing rules · `rule-why-fired` — explain a match."################;

pub(super) const RULE_GAP_SKILL_MD: &str = r################"---
name: rule-gap
description: Find review-feedback patterns your team repeats that aren't captured as DiffLore rules yet. Use in PR retros, after a batch of review comments, or when the user asks "what are we missing as a rule?".
---

# Rule Gap Analysis

Surface patterns that should be rules but aren't — the highest-leverage captures to add.

### 1. Pull review signals (per topic)

```text
get_past_verdicts(query="<natural-language topic>", file="<optional-path>")
```

Returns `{title, body, file_patterns}` memories — including dismissed
("tried but rejected") ones.

Treat repeated explicit corrections, review comments, failed-fix patterns, and
durable preferences as gap signals. A user explicitly asking "save this rule"
can be captured immediately with `remember-rule-guide`; everything else needs a
repeated pattern before becoming a proposed rule.

### 2. Diff against the current library

```text
resource: difflore://rules/active        # this project's library as Markdown
```

For each memory, check whether an existing rule already covers its topic +
`file_patterns`. Cross-check with `difflore ask "Do we already have guidance for
<topic>?"` or `get_rules(ids=[...])` on suspicious matches.

### 3. Propose captures

Cluster yourself; pick 3-5 patterns that repeat across **3+ memories** with no
covering rule. For each, propose the capture shape: action-phrased title,
matching `file_patterns`, trigger, minimal bad/good, and 1-2 source examples.
Only call `remember_rule(...)` after the user approves a proposal or explicitly
asks you to save all of them.

## Avoid

- Don't propose rules for single-occurrence memories (3+ is the bar).
- Don't duplicate an existing strong match.
- Don't propose vague rules ("write better tests") — actionable or it's noise.
- Don't turn every preference into a rule; it must affect future coding choices."################;

pub(super) const RULE_DIFF_SKILL_MD: &str = r################"---
name: rule-diff
description: Summarize team rule changes since the last `difflore cloud sync`. Use after a sync, when the user asks "what's new from the team?", or before a review session.
---

# Rule Diff

Show what changed in the team rule set since the last sync — added, strengthened, removed.

### 1. Read the snapshot

```text
resource: difflore://rules/active        # Markdown; _meta.synced_at in frontmatter
```

Compare `_meta.synced_at` against the previous snapshot (conversation history, or ask the user).

### 2. Present grouped by change

```
Team rule changes since <last_sync>:
added (3)
  * [pr_review]  "no router-core in adapters"   — PR #421
strengthened (2)
  * [manual]     "always Arc for shared state"  0.75 → 0.82
removed (1)
  * [manual]     "use async_std for I/O"        — superseded
```

Prioritize `pr_review` / `cloud` additions (team-visible) over personal conversation captures.

## Avoid

- Don't list unchanged rules — only changes.
- Don't reorder by internal score — stable added/changed/removed grouping scans easier.
- Don't call `sync` yourself; the user runs `difflore cloud sync` first."################;

pub(super) const RULE_WHY_FIRED_SKILL_MD: &str = r################"---
name: rule-why-fired
description: Explain why a specific DiffLore rule matched the current file / diff. Use when the user asks "why is this rule here?", disputes a rule, or debugs a false-positive match.
---

# Rule: Why Fired?

Give a concrete, history-based reason a rule showed up — not abstract ML talk.

```text
get_rules(ids=["<rule-id>"])      # or reuse the search_rules match reasons
```

Explain in priority order; the first that applies is usually *the* reason:

1. **File-pattern match** (strongest) — the rule's `file_patterns` glob matches
   the edited path. Cite it: `["src/**/*.rs"]` vs `src/worker/executor.rs`.
2. **Semantic similarity** — the rule body's topic matches the current
   intent/code. Cite the concrete phrase, not the score.
3. **Past-verdict recall** — `get_past_verdicts` returned historical judgments on
   the same pattern, pulling the rule along.

Add `rule_timeline(rule_id="<id>")` to ground the story in dated history rows.
If none apply cleanly, say "borderline match" — never fabricate a reason.

## Avoid

- Don't explain in ML abstractions — cite "line X matches glob Y".
- Don't dismiss a dispute with "the rule is always right." If the user says it
  doesn't apply, they're probably right — check `rule_timeline`; if confirmed
  bad, say it should be removed via the team memory admin path.
- Don't walk the whole retrieval stack unless asked."################;

pub(super) const RULE_JOURNEY_SKILL_MD: &str = r################"---
name: rule-journey
description: Summarize the local DiffLore rule library for onboarding, retros, or repo memory review.
---

# Rule Journey

Summarize how a repo's DiffLore rules are evolving.

Prefer public surfaces: `difflore status`, `difflore recall`, `difflore ask`,
MCP `search_rules` / `rule_timeline`. Inspect `~/.difflore/data.db` only if asked
for a deeper report — and don't dump raw rows.

Write a concise Markdown report (default `./difflore-rule-summary.md`):

1. Rule count and date range.
2. Main origins (manual / conversation / pr_review).
3. Rules with the strongest team review history, and why they matter.
4. File-pattern coverage gaps.
5. Next steps (import reviews, capture missing rules).

## Avoid

- Don't write marketing copy or cite benchmarks unless asked.
- Don't promise cloud analytics to a local-only user.
- Don't produce a long report when a short onboarding summary fits."################;

pub(super) const SMART_EXPLORE_SKILL_MD: &str = r################"---
name: smart-explore
description: Map a repository cheaply before reading files. Use when starting in an unfamiliar repo, before large refactors, or when deciding which files and DiffLore rules to inspect first.
---

# Smart Explore

Build a small repo map before spending context on file reads — cheap shell plus DiffLore rule lookup.

### 1. Cheap map

```bash
rg --files | rg "^(src|crates|packages|apps|tests|docs)/"   # sample large repos
```

Note file-type counts, the directories holding most of the source, orientation
files (README / AGENTS / Cargo.toml / package.json), and git working-tree changes.

### 2. Query rules before reading deeply

```text
search_rules(intent="<hint>", file="<path>", top_k=5)
get_rules(ids=["<id>"])                                  # only relevant ones
rule_timeline(rule_id="<id>", depth_before=3, depth_after=3)   # borderline / old rules
```

### 3. Read narrowly

Read the smallest useful queue first — changed files, nearby tests, one metadata
file — before fanning out. Need more? `rg --files | rg "auth|review|provider"`,
then repeat step 2 with the concrete path.

If exploration shows a repeated gap and no rule covers it, use `rule-gap` to
propose a capture or `remember-rule-guide` when the user explicitly asks to save
one. Do not invent an active rule in the summary.

**Read Gate:** a `DiffLore Read Gate` block before a big read is soft orientation —
apply the shown rules, use the suggested `rule_timeline` calls, and read the file
only when you need exact lines. It never means "don't read".

## Avoid

- Don't read a whole directory just because it's large.
- Don't call `get_rules` before `search_rules`.
- Don't lead with heavy static analysis.
- Don't treat the map as authoritative architecture — it's triage."################;

pub(super) const KNOWLEDGE_AGENT_SKILL_MD: &str = r################"---
name: knowledge-agent
description: Answer broad questions from the team's DiffLore codebase rules — repo decisions, review history, team rules. Use for a focused "brain" over many rules at once.
---

# Knowledge Agent

Answer cross-cutting questions over DiffLore memory. Use `difflore ask` for the
public path; reach for MCP tools only when you need provenance or full bodies.

**Not for:** single-rule lookup (`rule-search`), capturing a rule
(`remember-rule-guide`), or diff since sync (`rule-diff`).

### 1. Ask the public CLI

```bash
difflore ask "What are our conventions for async task boundaries?"
difflore ask "What should I know before changing this worker?" --file src/worker.rs
```

If answers are empty, suggest `difflore init` + `difflore import-reviews`.

### 2. Ground important claims

```text
search_rules(intent="<topic>", file="<optional-path>", top_k=5)
get_rules(ids=["<id>"])                                # 1-3 that matter
rule_timeline(rule_id="<id>", depth_before=5, depth_after=5)
```

### 3. Report

Short answer · supporting rule IDs/titles · any gap, stale signal, or conflict ·
the next public command (`difflore recall` / `ask` / `status`).
Call gaps "missing coverage" or "candidate captures", not active rules, unless a
command output proves they were approved.

## Avoid

- Don't treat `ask` as authoritative with zero citations — verify via `search_rules`.
- Don't pass cloud tokens or secrets through the conversation.
- Don't build a global summary when the user named a specific file/subsystem — scope first.
- Don't call retired corpus or knowledge subcommands.
- Don't invent "learned N rules" or value receipts; quote the command output."################;

pub(super) const MEMORY_CANDIDATE_TRIAGE_SKILL_MD: &str = r################"---
name: memory-candidate-triage
description: Help a user inspect and triage DiffLore memory candidates without approving or rejecting them yourself.
---

# Memory Candidate Triage

Use this when the user asks what DiffLore learned, which candidate memories
exist, what should be approved, or why a memory is or is not active.

## Flow

1. Read the inventory with `list_memory(state="pending", limit=100)` or the
   `difflore://memory/inbox` resource.
2. For any item you might recommend, call `get_memory_item(id="<item-id>")`
   before judging it.
3. Group items into:
   - approve: specific, reusable, scoped, and not a duplicate
   - merge/rewrite: valuable but overlapping, vague, too broad, or missing
     useful `file_patterns`
   - reject/defer: one-off, noisy, stale, unsafe, or not a coding rule
4. Look for consolidation: when several candidates say the same thing, pick one
   canonical wording and list the duplicate candidate ids to reject or rewrite.
5. Explain that only active rules affect agents. Drafts and candidates do not.
   `pending` means saved for review, not failed learning.
6. Give the exact CLI commands for the user to run, such as
   `difflore memory approve draft:<id>` or
   `difflore memory reject session:<hash>`.
7. Treat `pending` as successfully saved for user review, not as a failed
   capture. Do not call `remember_rule` again for the same persisted draft.

## Guardrails

- Do not approve, reject, sync, archive, delete, or edit memory through MCP.
- Do not claim a candidate affected code. Use `get_memory_activity` only for
  retrieved/surfaced evidence, not proof of final-code influence.
- Do not retry or duplicate an existing pending draft just because it is not
  active yet.
- Do not read local SQLite files unless the user explicitly asks; use DiffLore
  CLI/MCP surfaces first.
- Keep recommendations scoped and reversible; prefer "merge/rewrite" over
  approving vague rules.
- Never invent a "learned N rules" receipt. Quote the inventory or command
  output that proves the candidate exists."################;

pub(super) const PRE_SUBMIT_REVIEW_SKILL_MD: &str = r################"---
name: pre-submit-review
description: Use when a user is about to commit, push, open a PR, submit code, or asks for a final local review after editing in a DiffLore repo.
---

# Pre-Submit Review

Before code leaves the working tree, run a local DiffLore review pass and fix
only the issues it finds. This is a delivery gate, not a commit command.

## Flow

1. Inspect the current diff: `git diff --stat` and, if useful, `git status --short`.
2. Run `difflore review --diff all`.
3. If the review reports findings, summarize them and ask before applying broad changes.
4. Fix locally with the current agent, or run `difflore fix` when interactive patching is appropriate.
5. Run `difflore review --diff all` again until it is clean or remaining items are explicitly deferred.
6. Show the user the final status and tell them to review `git diff`.

## Guardrails

- Do not commit, push, open a PR, or post review comments.
- Do not run `difflore fix --yes` unless the user explicitly asked for automatic fixes.
- Do not treat "could not review" as clean; resolve review provider/config issues or report the blocker.
- Keep fixes scoped to DiffLore findings and the user's requested change.

## Useful Commands

```bash
difflore review --diff all
difflore fix
difflore status
```

When nothing is found, say the DiffLore pre-submit review is clean and mention
that the user should still review the final diff before committing."################;

pub(super) const SESSION_RECAP_SKILL_MD: &str = r################"---
name: session-recap
description: After editing code in a difflore repo, optionally end your final summary with one quiet line when accepted edits were captured for this task. Use when wrapping up a coding task.
---

# Session Recap

When you finish editing code, add **one line** to your wrap-up only when
difflore captured accepted edits for this task:

```bash
difflore status   # read accepted edits, not the heading
```

> difflore: 2 accepted edits captured.

## Rules

- **Numbers come from `difflore status`. Never invent them.**
- Do not prefix the line with `session-recap:` or the skill name.
- If you use a label, write lowercase `difflore:`.
- **Nothing applied or only recall/agent-ready activity? Say nothing.**
- Mention pending captures only if this task created them and the command output
  gave concrete ids; label them as pending review, not active agent behavior.
- Do not mention "top memory", "best memory", recall counts, ready-for-agent
  counts, or "no accepted edits yet" in the recap line.
- Name a source repo only if you are citing a specific memory that directly
  shaped the edit in your main summary, not as a generic recap metric.
- Do not translate accepted edits into saved time, ROI, avoided comments, or
  reduced review rework.
- One line. Once per session. Not a pitch."################;

pub(super) const DIFFLORE_ONBOARD_SKILL_MD: &str = r################"---
name: difflore-onboard
description: Guide a user through first local difflore value in a repo: init, import PR review memory locally, preview recall, and report receipts after each step.
---

# DiffLore Onboard

Use this when the user wants to start using DiffLore in a repo, verify that it is wired, or get from a cold checkout to the first useful recall.

## Flow

1. Confirm you are in the intended git repo.
2. Run `difflore init`.
3. Run `difflore import-reviews --dry-run`.
4. If the dry run is healthy, run `difflore import-reviews`.
5. Run `difflore recall --diff`.
6. End with a concrete `difflore status` receipt. Only call it value when accepted edits were actually captured.

## Receipts

After every write step, echo the concrete receipt line DiffLore printed, such as:

- `+N local memory writes`
- `+1 rule captured from agent chat`
- `+N accepted edits recorded for local value tracking`

If a command writes nothing, say what the next command is and do not invent value numbers.
If a command creates pending candidates, say they were saved for review and are
not active rules until approved. Never report "N learnings" unless the command
printed that number.

## Upgrade Path

Keep local memory first. Cloud login, upload, and team sync are upgrades:

- Use `difflore cloud login` only when the user asks for team sync or multi-device memory.
- Use `difflore import-reviews --upload` only after the user has opted into cloud processing.
- Existing local conversation captures and imported candidates stay local unless explicitly synced."################;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn normalized_doc(text: &str) -> String {
        text.replace("\r\n", "\n")
            .trim_end_matches(['\r', '\n'])
            .to_owned()
    }

    #[test]
    fn generated_skill_docs_match_root_plugin_files() {
        let skills_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin/skills");
        if !skills_root.exists() {
            return;
        }

        for (slug, embedded) in [
            ("rule-search", RULE_SEARCH_SKILL_MD),
            ("rule-gap", RULE_GAP_SKILL_MD),
            ("rule-diff", RULE_DIFF_SKILL_MD),
            ("rule-why-fired", RULE_WHY_FIRED_SKILL_MD),
            ("rule-journey", RULE_JOURNEY_SKILL_MD),
            ("smart-explore", SMART_EXPLORE_SKILL_MD),
            ("knowledge-agent", KNOWLEDGE_AGENT_SKILL_MD),
            ("memory-candidate-triage", MEMORY_CANDIDATE_TRIAGE_SKILL_MD),
            ("pre-submit-review", PRE_SUBMIT_REVIEW_SKILL_MD),
            ("session-recap", SESSION_RECAP_SKILL_MD),
            ("difflore-onboard", DIFFLORE_ONBOARD_SKILL_MD),
        ] {
            let path = skills_root.join(slug).join("SKILL.md");
            let expected = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("could not read {}: {err}", path.display()));
            assert_eq!(
                normalized_doc(embedded),
                normalized_doc(&expected),
                "{} drifted from generated MCP resource",
                path.display()
            );
        }
    }
}
