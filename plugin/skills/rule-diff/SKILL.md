---
name: rule-diff
description: Summarize team rule changes since the last `difflore cloud sync`. Use right after a sync completes, when the user asks "what's new from the team?", or when pushing local captures has just happened.
---

# Rule Diff

Show what's changed in the team rule set since the local cache was last synced - new additions, strengthened rules, and removals. Useful for "catch me up" moments after a sync.

## When to Use

- Immediately after `difflore cloud sync` completes (user or automation just ran it)
- User asks "what's new from the team?" / "did anything change?"
- Before a PR review session - so you apply rules the team added since last time

## 2-Step Recipe

### Step 1: Read the active rule snapshot

```
# MCP resource (static, no tool call needed)
Read resource: difflore://rules/active
```

The resource returns the current rule set rendered as Markdown with `_meta.synced_at` embedded in the frontmatter. Compare `_meta.synced_at` against the previous snapshot (look in conversation history or ask the user).

### Step 2: Group changes and present

Produce a compact diff summary grouped by origin:

```
Team rule changes since <last_sync>:

added (3)
  * [pr_review]  "no router-core in adapters"   - extracted from PR #421
  * [pr_review]  "use Mapping not dict for headers"   - PR #418
  * [cloud]      "ban .unwrap() in hot paths"

strengthened (2)
  * [manual]     "always Arc for shared state"   0.75 -> 0.82
  * [extracted]  "prefer PathBuf over String"    0.68 -> 0.74

removed (1)
  * [manual]     "use async_std for I/O"         - superseded
```

Prioritize pr_review and cloud-origin additions (team-visible) over conversation captures (personal). Highlight additions with strong team review history - those are the rules agents will use often.

## Anti-patterns

- **Don't** list every single rule - only changes. The user already has the full library.
- **Don't** reorder by internal score - stable grouping (added / changed / removed) is easier to scan.
- **Don't** silently call `sync` yourself. If the user wants a fresh diff, they run `difflore cloud sync` explicitly first.

## Related

- `rule-search` - look up the full body of any changed rule
- CLI: `difflore cloud sync` (call it yourself if the user explicitly asks)
