---
name: knowledge-agent
description: Answer broad questions from the team's DiffLore review memory — repo conventions, review history, team decisions. Use for a focused "brain" over many rules at once.
---

# Knowledge Agent

Answer cross-cutting questions over DiffLore memory. Use `difflore ask` for the
public path; reach for MCP tools only when you need provenance or full bodies.

**Not for:** single-rule lookup (`rule-search`), capturing a rule
(`remember-rule-guide`), or diff since sync (`rule-diff`).

### 1. Ask the public CLI

```bash
difflore ask "What are our conventions for async task boundaries?"
difflore ask "What should I know before changing this worker?" --file src/worker.rs
```

If answers are empty, suggest `difflore init` + `difflore import-reviews`.

### 2. Ground important claims

```text
search_rules(intent="<topic>", file="<optional-path>", top_k=5)
get_rules(ids=["<id>"])                                # 1-3 that matter
rule_timeline(rule_id="<id>", depth_before=5, depth_after=5)
```

### 3. Report

Short answer · supporting rule IDs/titles · any gap, stale signal, or conflict ·
the next public command (`difflore recall` / `ask` / `status`).

## Avoid

- Don't treat `ask` as authoritative with zero citations — verify via `search_rules`.
- Don't pass cloud tokens or secrets through the conversation.
- Don't build a global summary when the user named a specific file/subsystem — scope first.
