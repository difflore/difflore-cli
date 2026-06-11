---
name: smart-explore
description: Map a repository cheaply before reading files. Use when starting in an unfamiliar repo, before large refactors, or when deciding which files and DiffLore rules to inspect first.
---

# Smart Explore

Build a small repo map before spending context on file reads — cheap shell plus DiffLore rule lookup.

### 1. Cheap map

```bash
rg --files | rg "^(src|crates|packages|apps|tests|docs)/"   # sample large repos
```

Note file-type counts, the directories holding most of the source, orientation
files (README / AGENTS / Cargo.toml / package.json), and git working-tree changes.

### 2. Query rules before reading deeply

```text
search_rules(intent="<hint>", file="<path>", top_k=5)
get_rules(ids=["<id>"])                                  # only relevant ones
rule_timeline(rule_id="<id>", depth_before=3, depth_after=3)   # borderline / old rules
```

### 3. Read narrowly

Read the smallest useful queue first — changed files, nearby tests, one metadata
file — before fanning out. Need more? `rg --files | rg "auth|review|provider"`,
then repeat step 2 with the concrete path.

**Read Gate:** a `DiffLore Read Gate` block before a big read is soft orientation —
apply the shown rules, use the suggested `rule_timeline` calls, and read the file
only when you need exact lines. It never means "don't read".

## Avoid

- Don't read a whole directory just because it's large.
- Don't call `get_rules` before `search_rules`.
- Don't lead with heavy static analysis.
- Don't treat the map as authoritative architecture — it's triage.
