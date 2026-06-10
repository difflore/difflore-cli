---
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
- Do not produce a long report when a short onboarding summary is enough.
