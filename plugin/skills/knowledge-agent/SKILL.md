---
name: knowledge-agent
description: Answer broad questions from the team's DiffLore review memory using the current public CLI and MCP rule tools. Use when the user asks for a focused "brain" over repo conventions, review history, or team decisions.
---

# Knowledge Agent

Answer cross-cutting questions from DiffLore memory without exposing retired
CLI surfaces. Use `difflore ask` for the public CLI path, and use MCP rule
tools only when you need tighter provenance or full rule bodies.

## When to Use

- User wants a domain-scoped answer: "summarize everything we've decided about hooks", "what's our policy on unwrap()?"
- Team retro / onboarding: collect the relevant rules and explain the pattern.
- Cross-cutting investigation across many rules where reading them one-by-one would burn too much context.

**When NOT to use**:

- Single rule lookup -> use the `rule-search` skill.
- Capturing a new rule -> use `remember-rule-guide`.
- Diff since last sync -> use `rule-diff`.

## Prerequisites

1. **DiffLore CLI on PATH**: `difflore --version` should succeed.
2. **Local memory exists**: if answers are empty, suggest `difflore init` and
   `difflore import-reviews`.
3. **Cloud is optional**: use `difflore cloud status` only when the user asks
   about team sync or cloud-backed governance.

## Workflow

### Step 1: Ask the public CLI

```bash
difflore ask "What are our review conventions for async task boundaries?"
```

Use `--file <PATH>` when the question is tied to a concrete file:

```bash
difflore ask "What should I know before changing this worker?" --file src/worker.rs
```

For scripts, add `--json` and summarize the answer plus cited rules.

### Step 2: Ground important claims

If the answer needs citations or the user asks "why?", use the MCP tools:

```
search_rules(intent="<topic>", file="<optional-path>", top_k=5)
get_rules(ids=["<id-1>", "<id-2>"])
rule_timeline(rule_id="<id>", depth_before=5, depth_after=5)
```

Keep full-body fetches to the 1-3 rules that actually matter.

### Step 3: Report with caveats

Give the user:

- the short answer
- the rule IDs or titles that support it
- any obvious gap, stale signal, or conflicting guidance
- the next public command if they need to inspect locally (`difflore recall`,
  `difflore ask`, `difflore status`, or `difflore cloud status`)

## Anti-patterns

- **Don't** call retired corpus or knowledge subcommands.
- **Don't** treat `ask` as authoritative when it returns no citations; verify
  with `search_rules` before making strong claims.
- **Don't** pass cloud tokens or environment secrets through the conversation.
- **Don't** build global summaries when the user gave a specific file or
  subsystem; scope the query first.

## Related

- `rule-search` skill — single-rule lookup.
- `rule-gap` skill — find repeated review patterns that are not covered yet.
- `rule-journey` skill — produce a longer narrative report from the local DB.
