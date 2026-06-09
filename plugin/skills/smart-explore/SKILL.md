---
name: smart-explore
description: Map a repository cheaply before reading files. Use when starting work in an unfamiliar repo, before large refactors, or when deciding which files and DiffLore rules to inspect first.
---

# Smart Explore

Build a small repo map before spending context on file reads. This skill uses cheap shell inspection plus DiffLore MCP rule lookup; it does not rely on a separate user-facing explorer command.

## When to Use

- The user asks you to inspect, understand, audit, or modify an unfamiliar codebase.
- You see a large repo and need to choose which files to read first.
- You are about to run `search_rules` but do not yet know the relevant file, directory, or subsystem.
- You need a quick changed-file map before review or implementation.

## Workflow

### Step 1: Run a cheap map

```bash
rg --files
```

For very large repos, sample by likely source and test directories:

```bash
rg --files | rg "^(src|crates|packages|apps|tests|docs)/"
```

Build a compact map from:

- extension-only file type counts
- directories carrying most of the source surface
- orientation files such as README, AGENTS, Cargo.toml, package.json
- git working tree changes
- the smallest useful file queue
- ready-to-run `search_rules` hints

### Step 2: Query rules before reading deeply

For each changed or high-value file, ask DiffLore for local conventions:

```
search_rules(intent="<hint.query>", file="<hint.file>", top_k=5)
```

Only expand IDs that look relevant:

```
get_rules(ids=["<id-1>", "<id-2>"])
```

Use `rule_timeline` for borderline rules whose age or provenance matters:

```
rule_timeline(rule_id="<id>", depth_before=3, depth_after=3)
```

### Step 3: Read narrowly

Read the small file queue before fanning out. Prefer changed files, nearby tests, and one build or project metadata file over broad recursive reads.

When you need more files, search by subsystem:

```bash
rg --files | rg "auth|review|provider"
```

Then repeat Step 2 with the concrete file path.

## Output Discipline

When reporting back to the user, summarize:

- the project shape in one or two sentences
- the files you will inspect first
- any rules that look relevant
- the next concrete action

Do not dump raw file lists unless the user asks.

## Read Gate Interaction

DiffLore may also surface a `DiffLore Read Gate` block before a large file read. Treat it as a cheap orientation layer:

- Apply the shown rules before forming an edit plan.
- Use the suggested `rule_timeline(...)` calls when provenance or staleness matters.
- Continue with the file read when exact implementation details or line numbers are needed.

The gate is soft. It never means "do not read this file"; it means "you may not need to spend that context yet."

## Anti-patterns

- Do not read a whole directory just because it is large.
- Do not call `get_rules` before `search_rules`.
- Do not run heavy static analysis as the first move.
- Do not treat the cheap map as authoritative architecture analysis; it is triage, not a parser.
