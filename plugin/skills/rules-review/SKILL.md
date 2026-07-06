---
name: rules-review
description: Review a diff or PR with the team's DiffLore rules as the review criteria. Use when the user asks to review changes with team rules, or invokes /rules-review.
---

# Rules Review

Review changes with the team's approved rules as authoritative criteria —
not generic best practices.

## Flow

1. Collect the changed files (`git diff --name-only <base>...HEAD`, staged, or
   the files the user pointed at).
2. Recall the rules that apply — group files by area, one search per area:

```text
search_rules(intent="<what this change does>", file="src/billing/retry.ts", top_k=5)
get_rules(ids=["conv-a1f9c"], file="src/billing/retry.ts")   # only the ones that apply
```

3. Review the diff. Matched rules are the team's review rules — treat them as
   authoritative review criteria, ahead of generic style opinions.
4. For each rule-backed finding, cite the rule and its provenance line
   (`<rule title> ← learned from <repo/PR/session>`), then the file:line.
5. Findings no rule covers are normal review judgment — report them separately
   so rule-backed and generic findings stay distinguishable.

## Deterministic gate

For a metered, CI-identical verdict, run the CLI gate instead of (or after)
this skill: `difflore review --pr <N>` or `difflore review --diff all`.
Every rule-backed catch it reports is recorded locally.

## Avoid

- Don't dump the whole rule library into context — recall per changed area only.
- Don't soften a matched rule because the code "looks fine"; if the rule is
  wrong or stale, say so and point to `difflore rules review` to amend it.
- Don't claim a finding is rule-backed when no rule matched.

## Related

`rule-search` — targeted recall · `rule-why-fired` — explain a match · `rule-gap` — a convention the library is missing.
