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

### 2. Diff against the current library

```text
resource: difflore://rules/active        # full library as Markdown
```

For each memory, check whether an existing rule already covers its topic +
`file_patterns`. Cross-check with `difflore ask "Do we already have guidance for
<topic>?"` or `get_rules(ids=[...])` on suspicious matches.

### 3. Propose captures

Cluster yourself; pick 3-5 patterns that repeat across **3+ memories** with no
covering rule. For each, propose a `remember_rule(...)` — action-phrased title,
matching `file_patterns`, minimal bad/good. Present to the user; don't auto-capture.

## Avoid

- Don't propose rules for single-occurrence memories (3+ is the bar).
- Don't duplicate an existing strong match.
- Don't propose vague rules ("write better tests") — actionable or it's noise."################;

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
description: Answer broad questions from the team's DiffLore review memory — repo conventions, review history, team decisions. Use for a focused "brain" over many rules at once.
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

## Avoid

- Don't treat `ask` as authoritative with zero citations — verify via `search_rules`.
- Don't pass cloud tokens or secrets through the conversation.
- Don't build a global summary when the user named a specific file/subsystem — scope first.
- Don't call retired corpus or knowledge subcommands."################;

pub(super) const SESSION_RECAP_SKILL_MD: &str = r################"---
name: session-recap
description: After editing code in a DiffLore repo, end your final summary with one line on what DiffLore contributed — memories applied, their source repos, review-time saved. Surfaces value that is otherwise silent. Use when wrapping up a coding task.
---

# Session Recap

When you finish editing code, add **one line** to your wrap-up:

```bash
difflore status   # read the "Value (last Nd)" section
```

> 📋 DiffLore: 3 team memories shaped these edits — e.g. "no Promise.race
> timeout" ← vitejs/vite. ~12 review-minutes saved. `difflore status` for more.

## Rules

- **Numbers come from `difflore status`. Never invent them.**
- **Nothing applied? Say nothing.** No "0 memories" line.
- Name a source repo only when a surfaced memory showed `learned from <repo>`.
- If status says "recall-to-edit loop not captured yet", don't claim the saved
  minutes are fully memory-driven — say "review patterns DiffLore tracks".
- One line. Once per session. Not a pitch."################;

pub(super) const DIFFLORE_ONBOARD_SKILL_MD: &str = r################"---
name: difflore-onboard
description: Guide a user through first local DiffLore value in a repo: init, import PR review memory locally, preview recall, and report receipts after each step.
---

# DiffLore Onboard

Use this when the user wants to start using DiffLore in a repo, verify that it is wired, or get from a cold checkout to the first useful recall.

## Flow

1. Confirm you are in the intended git repo.
2. Run `difflore init`.
3. Run `difflore import-reviews --dry-run`.
4. If the dry run is healthy, run `difflore import-reviews`.
5. Run `difflore recall --diff`.
6. End with the `Value (last Nd)` line from `difflore status`.

## Receipts

After every write step, echo the concrete receipt line DiffLore printed, such as:

- `+N local memory writes`
- `+1 rule captured from agent chat`
- `+N accepted edits recorded for local value tracking`

If a command writes nothing, say what the next command is and do not invent value numbers.

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
