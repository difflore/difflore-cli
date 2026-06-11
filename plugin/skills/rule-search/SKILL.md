---
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

`remember-rule-guide` — save a rule · `rule-gap` — find missing rules · `rule-why-fired` — explain a match.
