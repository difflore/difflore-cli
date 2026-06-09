---
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
- `rule-why-fired`: explain a specific match.
