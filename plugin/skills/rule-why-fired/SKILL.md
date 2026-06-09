---
name: rule-why-fired
description: Explain why a specific DiffLore rule matched the current file / diff. Use when the user asks "why is this rule here?", disputes a rule injection, or is debugging a false-positive match.
---

# Rule: Why Fired?

Give the user a precise, evidence-based answer for why a particular rule showed up in the agent's context.

## When to Use

- User asks "why is this rule injected?" / "why did you surface <rule-name>?"
- User disputes a rule's relevance ("this rule doesn't apply here")
- User is debugging retrieval quality or tuning the rule set

## 2-Step Recipe

### Step 1: Fetch the full rule body

```
get_rules(ids=["<rule-id>"])
```

If the rule came back from `search_rules`, inspect its `evidence` array first. That response now carries explicit evidence types:

- `filePatternMatch` when the current file matches the rule's glob(s)
- `semanticSimilarity` for the retrieval score against the user's intent

Look for three signals in the response:

- `file_patterns` — glob(s) the rule claims to apply to. A match here is the **strongest** reason a rule fired.
- `similarity` (if present in the response metadata) — semantic match against the current file / intent.
- `origin` + `confidence` — higher-confidence rules have a lower injection threshold.

### Step 2: Explain the match in 3 ordered causes

Present the reasoning in this priority order — the first reason that applies is usually *the* reason:

1. **File-pattern match** (strongest):
   ```
   Rule `conv-a1f9c` has file_patterns ["src/**/*.rs"].
   You are editing src/worker/executor.rs → pattern matches.
   This is a strict file-pattern match.
   ```

2. **Semantic similarity** (next):
   ```
   Rule body mentions "executor boundary" / "borrow across spawn".
   Current file contains `tokio::spawn(self.process(...))` — matches
   the retrieval query with cosine similarity ~0.82.
   ```

3. **Past-verdict recall** (occasional):
   ```
   get_past_verdicts returned 2 historical judgments on the same pattern
   in this repo — the agent transport's strict-verdict path pulled the associated
   rule along for context.
   ```

When `rule_timeline` is available, use it to ground the story with
chronological evidence rows. Those rows now carry their own `evidence`
objects, so you can cite both the event and the reason it matters instead
of reconstructing provenance from free-form prose.

If none of the three apply cleanly, say so plainly — "this looks like a borderline match; retrieval confidence is N.NN." Never fabricate a reason.

## Anti-patterns

- **Don't** explain in abstract ML terms — the user wants a concrete citation ("line X matches glob Y").
- **Don't** dismiss the dispute with "the rule is always right." If the user says it doesn't apply, they're probably correct — inspect provenance with `rule_timeline`; if it is confirmed bad, tell the user the rule should be removed through the current governance path.
- **Don't** walk through the full retrieval stack unless asked. Three clear causes beats a tutorial.

## Related

- `rule-search` — inspect the retrieved rule set for the current file
- Public CLI: `difflore recall` / `difflore ask` can verify what context the agent sees; provenance inspection uses the MCP `rule_timeline` tool.
