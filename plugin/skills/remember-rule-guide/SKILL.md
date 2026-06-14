---
name: remember-rule-guide
description: Capture a local DiffLore rule when the user explicitly asks to remember a repo convention or correction.
---

# Remember Rule

Only when the user clearly wants a rule saved — "remember this", "save this as a
rule", "for this repo always X", "don't do X again", "from now on use Y". If the
wording is ambiguous, ask one short confirmation first.

```text
remember_rule(
  title="short actionable rule",
  body="fuller explanation with context",
  file_patterns=["src/**/*.rs"],     # narrow when file/subsystem-specific
  severity="low|medium|high",
  bad_code="optional: what to avoid",
  good_code="optional: preferred form",
)
```

After saving, tell the user it's local and checkable with `difflore recall` / `difflore ask`.

## Avoid

- Don't capture vague complaints — make them actionable first.
- Don't save secrets, private chat content, or broad project history.
- Don't duplicate an existing rule — search first if unsure.
- Don't capture when the user hasn't asked to save a rule.
