// Generated from `plugin/skills/**/SKILL.md` so the published `difflore-core`
// crate can embed MCP skill resources without keeping a second skill tree.
// Update by regenerating from the root plugin skill files.
#![allow(clippy::needless_raw_string_hashes)]

pub(super) const RULE_SEARCH_SKILL_MD: &str = r################"---
name: rule-search
description: Search the local DiffLore rule library before editing code, reviewing a diff, or answering a repo-convention question.
---

# Rule Search

Use this skill when the user is editing code, reviewing a diff, or asking about
repo conventions.

## Workflow

1. Call `search_rules` with the current intent and file path.
2. If a result is borderline or disputed, inspect `rule_timeline` for context.
3. Fetch full bodies with `get_rules` only for the 1-3 rules that actually
   apply.

```text
search_rules(intent="async executor boundary", file="src/worker.rs", top_k=5)
rule_timeline(rule_id="conv-a1f9c", depth_before=5, depth_after=5)
get_rules(ids=["conv-a1f9c"], file="src/worker.rs")
```

Skip `rule_timeline` when the top hit is clearly relevant. Use it when the user
asks why a rule exists, when confidence is low, or when a rule may be stale.

## Avoid

- Do not fetch full rule bodies before searching.
- Do not use very large `top_k` values unless the user asks for a broad audit.
- Do not re-query for every tool call; agent transports may already cache hits.

## Related

- `remember_rule`: save a new rule.
- `rule-gap`: find missing rules.
- `rule-why-fired`: explain a specific match."################;

pub(super) const RULE_GAP_SKILL_MD: &str = r################"---
name: rule-gap
description: Find review-feedback patterns your team keeps repeating that aren't yet captured as DiffLore rules. Use during PR retros, after a batch of review comments, or when the user asks "what are we missing as a rule?".
---

# Rule Gap Analysis

Surface review patterns that should be rules but aren't yet — the highest-leverage captures to add to your team's DiffLore library.

## When to Use

- User runs a retro and asks "what's next to codify?"
- A new PR batch came in and the user wants to convert comments into rules
- Onboarding a new codebase via `difflore import-reviews` and wanting to seed team rules

## 3-Step Recipe

### Step 1: Pull recent review feedback signals

For each topic the user wants to retro on, call:

```
get_past_verdicts(
  query="<topic-phrase>",
  file="<optional-path>"
)
```

Required `query` is a natural-language topic phrase ("async borrow", "header types"); `file` is an optional scoping hint. Run the call once per topic of interest. Returns past review extractions — each one is a structured `{ title, body, file_patterns, confidence }` tuple. Dismissed extractions still land here so we see the "tried but rejected" signal too.

### Step 2: Diff against the current rule library

Read the active rule library as a single Markdown document:

```
resource: difflore://rules/active
```

That resource renders the full library with origins and confidence — a one-shot listing without spending search tokens. Cross-reference with Step 1: for each extraction, check if any existing rule covers the same `file_patterns` + body topic.

Public CLI cross-check (use when you want to ask whether the current library
already covers the topic):

```bash
difflore ask "Do we already have review guidance for <topic>?"
```

For more precise matching, fetch specific rule full-text with `get_rules(ids=[...])` on any suspicious matches.

### Step 3: Propose remember_rule captures

For extractions without a covering rule:

1. Cluster by topic (do this yourself — a few lines of analysis).
2. Pick 3-5 high-leverage capture proposals where the pattern repeats across ≥3 extractions.
3. For each, propose a `remember_rule(...)` call with:
   - `title` rewritten for action ("don't X" → "always Y")
   - `file_patterns` that match where the extractions originated
   - `bad_code` / `good_code` if you can construct minimal examples

Present the proposals to the user and ask which to capture. Don't auto-capture — the user should steer.

## Anti-patterns

- **Don't** propose rules for single-occurrence extractions — wait for pattern repetition (≥3 is a reasonable bar).
- **Don't** duplicate existing rules. If `search_rules` returns a high-similarity match (>0.75), skip that cluster.
- **Don't** propose vague rules ("write better tests"). If the rule isn't actionable, it's noise.

## Related

- `remember-rule-guide` skill — schema for the capture call
- `rule-why-fired` skill — debug path when a proposed rule feels off
- Cloud: the team dashboard has the long-horizon signature-clustering pipeline for the same goal."################;

pub(super) const RULE_DIFF_SKILL_MD: &str = r################"---
name: rule-diff
description: Summarize team rule changes since the last `difflore cloud sync`. Use right after a sync completes, when the user asks "what's new from the team?", or when pushing local captures has just happened.
---

# Rule Diff

Show what's changed in the team rule set since the local cache was last synced — new additions, confidence bumps, and removals. Useful for "catch me up" moments after a sync.

## When to Use

- Immediately after `difflore cloud sync` completes (user or automation just ran it)
- User asks "what's new from the team?" / "did anything change?"
- Before a PR review session — so you apply rules the team added since last time

## 2-Step Recipe

### Step 1: Read the active rule snapshot

```
# MCP resource (static, no tool call needed)
Read resource: difflore://rules/active
```

The resource returns the current rule set rendered as Markdown with `_meta.synced_at` embedded in the frontmatter. Compare `_meta.synced_at` against the previous snapshot (look in conversation history or ask the user).

### Step 2: Group changes and present

Produce a compact diff summary grouped by origin:

```
Team rule changes since <last_sync>:

added (3)
  ● [pr_review]  "no router-core in adapters"   — extracted from PR #421
  ● [pr_review]  "use Mapping not dict for headers"   — PR #418
  ● [cloud]      "ban .unwrap() in hot paths"

confidence ↑ (2)
  ● [manual]     "always Arc for shared state"   0.75 → 0.82
  ● [extracted]  "prefer PathBuf over String"    0.68 → 0.74

removed (1)
  ● [manual]     "use async_std for I/O"         — superseded
```

Prioritize pr_review and cloud-origin additions (team-visible) over conversation captures (personal). Highlight high-confidence additions (>0.8) — those are the rules agents will inject often.

## Anti-patterns

- **Don't** list every single rule — only changes. The user already has the full library.
- **Don't** reorder by confidence — stable grouping (added / changed / removed) is easier to scan.
- **Don't** silently call `sync` yourself. If the user wants a fresh diff, they run `difflore cloud sync` explicitly first.

## Related

- `rule-search` — look up the full body of any changed rule
- CLI: `difflore cloud sync` (call it yourself if the user explicitly asks)"################;

pub(super) const RULE_WHY_FIRED_SKILL_MD: &str = r################"---
name: rule-why-fired
description: Explain why a specific DiffLore rule matched the current file / diff. Use when the user asks "why is this rule here?", disputes a rule injection, or is debugging a false-positive match.
---

# Rule: Why Fired?

Give the user a precise, evidence-based answer for why a particular rule showed up in the agent's context.

## When to Use

- User asks "why is this rule injected?" / "why did you surface <rule-name>?"
- User disputes a rule's relevance ("this rule doesn't apply here")
- User is debugging retrieval quality or tuning the rule set

## 2-Step Recipe

### Step 1: Fetch the full rule body

```
get_rules(ids=["<rule-id>"])
```

If the rule came back from `search_rules`, inspect its `evidence` array first. That response now carries explicit evidence types:

- `filePatternMatch` when the current file matches the rule's glob(s)
- `semanticSimilarity` for the retrieval score against the user's intent

Look for three signals in the response:

- `file_patterns` — glob(s) the rule claims to apply to. A match here is the **strongest** reason a rule fired.
- `similarity` (if present in the response metadata) — semantic match against the current file / intent.
- `origin` + `confidence` — higher-confidence rules have a lower injection threshold.

### Step 2: Explain the match in 3 ordered causes

Present the reasoning in this priority order — the first reason that applies is usually *the* reason:

1. **File-pattern match** (strongest):
   ```
   Rule `conv-a1f9c` has file_patterns ["src/**/*.rs"].
   You are editing src/worker/executor.rs → pattern matches.
   This is a strict file-pattern match.
   ```

2. **Semantic similarity** (next):
   ```
   Rule body mentions "executor boundary" / "borrow across spawn".
   Current file contains `tokio::spawn(self.process(...))` — matches
   the retrieval query with cosine similarity ~0.82.
   ```

3. **Past-verdict recall** (occasional):
   ```
   get_past_verdicts returned 2 historical judgments on the same pattern
   in this repo — the agent transport's strict-verdict path pulled the associated
   rule along for context.
   ```

When `rule_timeline` is available, use it to ground the story with
chronological evidence rows. Those rows now carry their own `evidence`
objects, so you can cite both the event and the reason it matters instead
of reconstructing provenance from free-form prose.

If none of the three apply cleanly, say so plainly — "this looks like a borderline match; retrieval confidence is N.NN." Never fabricate a reason.

## Anti-patterns

- **Don't** explain in abstract ML terms — the user wants a concrete citation ("line X matches glob Y").
- **Don't** dismiss the dispute with "the rule is always right." If the user says it doesn't apply, they're probably correct — inspect provenance with `rule_timeline`; if it is confirmed bad, tell the user the rule should be removed through the current governance path.
- **Don't** walk through the full retrieval stack unless asked. Three clear causes beats a tutorial.

## Related

- `rule-search` — inspect the retrieved rule set for the current file
- Public CLI: `difflore recall` / `difflore ask` can verify what context the agent sees; provenance inspection uses the MCP `rule_timeline` tool."################;

pub(super) const RULE_JOURNEY_SKILL_MD: &str = r################"---
name: rule-journey
description: Summarize the local DiffLore rule library for onboarding, retros, or repo memory review.
---

# Rule Journey

Use this skill when the user asks for a summary of how a repo's DiffLore rules
are evolving.

## Inputs

Prefer public DiffLore surfaces:

- `difflore status`
- `difflore recall`
- `difflore ask`
- MCP `search_rules`
- MCP `rule_timeline`

If direct local DB access is available and the user asked for a deeper report,
you may inspect `~/.difflore/data.db`, but keep the report focused and avoid
dumping raw SQL output.

## Report Shape

Write a concise Markdown report with:

1. Rule count and date range.
2. Main origins, such as manual, conversation, or PR review.
3. The highest-confidence rules and why they matter.
4. File-pattern coverage gaps.
5. Suggested next steps, such as importing reviews or capturing missing rules.

Default output path: `./difflore-rule-summary.md`, unless the user requests
another path.

## Avoid

- Do not write a marketing post unless the user asks for one.
- Do not cite benchmark numbers unless the user provides public results.
- Do not promise cloud analytics when the user is using only local DiffLore.
- Do not produce a long report when a short onboarding summary is enough."################;

pub(super) const SMART_EXPLORE_SKILL_MD: &str = r################"---
name: smart-explore
description: Map a repository cheaply before reading files. Use when starting work in an unfamiliar repo, before large refactors, or when deciding which files and DiffLore rules to inspect first.
---

# Smart Explore

Build a small repo map before spending context on file reads. This skill uses cheap shell inspection plus DiffLore MCP rule lookup; it does not rely on a separate user-facing explorer command.

## When to Use

- The user asks you to inspect, understand, audit, or modify an unfamiliar codebase.
- You see a large repo and need to choose which files to read first.
- You are about to run `search_rules` but do not yet know the relevant file, directory, or subsystem.
- You need a quick changed-file map before review or implementation.

## Workflow

### Step 1: Run a cheap map

```bash
rg --files
```

For very large repos, sample by likely source and test directories:

```bash
rg --files | rg "^(src|crates|packages|apps|tests|docs)/"
```

Build a compact map from:

- extension-only file type counts
- directories carrying most of the source surface
- orientation files such as README, AGENTS, Cargo.toml, package.json
- git working tree changes
- the smallest useful file queue
- ready-to-run `search_rules` hints

### Step 2: Query rules before reading deeply

For each changed or high-value file, ask DiffLore for local conventions:

```
search_rules(intent="<hint.query>", file="<hint.file>", top_k=5)
```

Only expand IDs that look relevant:

```
get_rules(ids=["<id-1>", "<id-2>"])
```

Use `rule_timeline` for borderline rules whose age or provenance matters:

```
rule_timeline(rule_id="<id>", depth_before=3, depth_after=3)
```

### Step 3: Read narrowly

Read the small file queue before fanning out. Prefer changed files, nearby tests, and one build or project metadata file over broad recursive reads.

When you need more files, search by subsystem:

```bash
rg --files | rg "auth|review|provider"
```

Then repeat Step 2 with the concrete file path.

## Output Discipline

When reporting back to the user, summarize:

- the project shape in one or two sentences
- the files you will inspect first
- any rules that look relevant
- the next concrete action

Do not dump raw file lists unless the user asks.

## Read Gate Interaction

DiffLore may also surface a `DiffLore Read Gate` block before a large file read. Treat it as a cheap orientation layer:

- Apply the shown rules before forming an edit plan.
- Use the suggested `rule_timeline(...)` calls when provenance or staleness matters.
- Continue with the file read when exact implementation details or line numbers are needed.

The gate is soft. It never means "do not read this file"; it means "you may not need to spend that context yet."

## Anti-patterns

- Do not read a whole directory just because it is large.
- Do not call `get_rules` before `search_rules`.
- Do not run heavy static analysis as the first move.
- Do not treat the cheap map as authoritative architecture analysis; it is triage, not a parser."################;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

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
        ] {
            let path = skills_root.join(slug).join("SKILL.md");
            let expected = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("could not read {}: {err}", path.display()));
            assert_eq!(
                embedded,
                expected.trim_end_matches(['\r', '\n']),
                "{} drifted from generated MCP resource",
                path.display()
            );
        }
    }
}
