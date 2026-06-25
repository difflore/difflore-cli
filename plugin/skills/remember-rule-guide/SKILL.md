---
name: remember-rule-guide
description: Full trigger guide for when to call the remember_rule MCP tool, with trigger phrases and anti-patterns.
---

# `remember_rule` - Full Trigger Guide

**Save and activate a local DiffLore memory rule from a coding rule the user explicitly asked to remember.** A direct user "remember this" request counts as approval, so fresh captures are active and served to agents immediately. This tool is the durable capture path; saying "got it, I'll remember" without calling it means the rule is lost the moment the conversation ends.

## MUST CALL when the user expresses intent like (in any language):

- "remember this" / "save this rule" / "note this down"
- "don't do X again" / "never do X" / "stop doing X"
- "from now on, X" / "going forward, X" / "whenever you write X, do Y"
- "add a rule that X" / "make a rule for X"
- "in this codebase we always X" / "our convention is X"
- "next time use X" / "I prefer X in this repo" when it changes future coding behavior

The trigger is the user's *intent* to make the rule stick across sessions - phrasing varies by language and tone, so match on intent, not exact words. If you're unsure, ask one short confirmation first.

## Call shape

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

## MUST NOT call when:

- The user is just asking a question or making an observation ("what does X do?")
- The user offered a one-off correction or preference for the current task but did not signal future use
- The preference is a one-off taste note, not durable coding guidance
- You inferred the rule from code review without the user saying so (use `search_rules` then `get_rules` for that flow)
- The user asked you to remember something that's NOT a coding rule (e.g. a meeting time)
- The rule is vague, non-actionable, or broad project history rather than coding guidance
- The content contains secrets or private chat content
- An existing rule already covers it - search first if unsure

## Capture the WHY in English

Capture the user's reasoning in `body` - the WHY, not just the what; the reasoning is what makes the rule useful. Write both `title` and `body` in English: if the user explained in another language, translate and summarise their point into clear English rather than pasting the original wording. Stay faithful to their meaning - don't drop the substance of the reason.

## After calling, confirm to the user

Echo the returned `item_id`, explain that it is active because the user explicitly asked to remember it, and show the CLI command to inspect it:

```bash
difflore memory show rule:<id>
```

If the `remember_rule` MCP tool is not available after tool discovery, use the CLI fallback instead:

```bash
difflore memory remember --title "<short actionable rule>" --body "<full context>" --file-pattern "src/**/*.rs" --json
```

The CLI fallback also treats the explicit user request as approval and saves an active rule. Tell the user you saved and enabled it.

If the tool says it strengthened an existing active rule, say that rule remains available to agents.

Do not reject, sync, archive, delete, or edit other memory through MCP. To undo an active remembered rule, tell the user they can run:

```bash
difflore memory disable rule:<id>
```
