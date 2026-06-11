---
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
- Don't walk the whole retrieval stack unless asked.
