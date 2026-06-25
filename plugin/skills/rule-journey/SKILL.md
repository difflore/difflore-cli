---
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
- Don't produce a long report when a short onboarding summary fits.
