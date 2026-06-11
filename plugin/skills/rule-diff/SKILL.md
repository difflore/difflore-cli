---
name: rule-diff
description: Summarize team rule changes since the last `difflore cloud sync`. Use after a sync, when the user asks "what's new from the team?", or before a review session.
---

# Rule Diff

Show what changed in the team rule set since the last sync — added, strengthened, removed.

### 1. Read the snapshot

```text
resource: difflore://rules/active        # Markdown; _meta.synced_at in frontmatter
```

Compare `_meta.synced_at` against the previous snapshot (conversation history, or ask the user).

### 2. Present grouped by change

```
Team rule changes since <last_sync>:
added (3)
  * [pr_review]  "no router-core in adapters"   — PR #421
strengthened (2)
  * [manual]     "always Arc for shared state"  0.75 → 0.82
removed (1)
  * [manual]     "use async_std for I/O"        — superseded
```

Prioritize `pr_review` / `cloud` additions (team-visible) over personal conversation captures.

## Avoid

- Don't list unchanged rules — only changes.
- Don't reorder by internal score — stable added/changed/removed grouping scans easier.
- Don't call `sync` yourself; the user runs `difflore cloud sync` first.
