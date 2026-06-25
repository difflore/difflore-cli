---
name: memory-candidate-triage
description: Help a user inspect and triage DiffLore memory candidates without approving or rejecting them yourself.
---

# Memory Candidate Triage

Use this when the user asks what DiffLore learned, which candidate memories
exist, what should be approved, or why a memory is or is not active.

## Flow

1. Read the inventory with `list_memory(state="pending", limit=100)` or the
   `difflore://memory/inbox` resource.
2. For any item you might recommend, call `get_memory_item(id="<item-id>")`
   before judging it.
3. Group items into:
   - approve: specific, reusable, scoped, and not a duplicate
   - merge/rewrite: valuable but overlapping, vague, too broad, or missing
     useful `file_patterns`
   - reject/defer: one-off, noisy, stale, unsafe, or not a coding rule
4. Look for consolidation: when several candidates say the same thing, pick one
   canonical wording and list the duplicate candidate ids to reject or rewrite.
5. Explain that only active rules affect agents. Drafts and candidates do not.
   `pending` means saved for review, not failed learning.
6. Give the exact CLI commands for the user to run, such as
   `difflore memory approve draft:<id>` or
   `difflore memory reject session:<hash>`.
7. Treat `pending` as successfully saved for user review, not as a failed
   capture. Do not call `remember_rule` again for the same persisted draft.

## Guardrails

- Do not approve, reject, sync, archive, delete, or edit memory through MCP.
- Do not claim a candidate affected code. Use `get_memory_activity` only for
  retrieved/surfaced evidence, not proof of final-code influence.
- Do not retry or duplicate an existing pending draft just because it is not
  active yet.
- Do not read local SQLite files unless the user explicitly asks; use DiffLore
  CLI/MCP surfaces first.
- Keep recommendations scoped and reversible; prefer "merge/rewrite" over
  approving vague rules.
- Never invent a "learned N rules" receipt. Quote the inventory or command
  output that proves the candidate exists.
