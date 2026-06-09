---
name: remember-rule-guide
description: Capture a local DiffLore rule when the user explicitly asks to remember a repo convention or correction.
---

# Remember Rule

Use this skill only when the user clearly wants a rule saved for future agent
sessions, for example:

- "remember this"
- "save this as a rule"
- "for this repo, always do X"
- "do not do X again"
- "from now on, use Y here"

If the user's wording is ambiguous, ask one short confirmation before calling
`remember_rule`.

## Capture

```text
remember_rule(
  title="short actionable rule",
  body="fuller explanation with context",
  file_patterns=["src/**/*.rs"],
  severity="low|medium|high",
  bad_code="optional example of what to avoid",
  good_code="optional example of preferred form",
)
```

Prefer narrow `file_patterns` when the rule is file- or subsystem-specific.
After capturing, tell the user the rule was saved locally and can be checked
with `difflore recall` or `difflore ask`.

## Avoid

- Do not capture vague complaints; turn them into actionable guidance first.
- Do not save secrets, private chat content, or broad project history.
- Do not duplicate an existing rule; search first if unsure.
- Do not silently capture when the user has not asked to save a rule.
