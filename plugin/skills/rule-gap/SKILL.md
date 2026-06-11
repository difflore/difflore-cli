---
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
- Don't propose vague rules ("write better tests") — actionable or it's noise.
