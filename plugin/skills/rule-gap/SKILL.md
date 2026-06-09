---
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
- Cloud: the team dashboard has the long-horizon signature-clustering pipeline for the same goal.
