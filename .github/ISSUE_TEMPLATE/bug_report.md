---
name: Bug report
about: Something is broken or behaves wrong
title: '[Bug] '
labels: bug
assignees: ''
---

**What you ran**

Command, agent integration, or workflow.

**What happened**

Paste the error, output, or screenshot.

**What you expected**

What should have happened instead.

**Environment**

Run `difflore doctor --report` and attach the generated report if possible.
Tokens and API keys are redacted.

- OS:
- `difflore --version`:
- Install method:
- Agent integration, if any:
- `gh auth status` works? yes/no/not relevant

**Reproduction**

Smallest steps that trigger the issue. Link or anonymize any repo/PR context.

**Logs**

If useful, rerun with:

```bash
RUST_LOG=debug difflore <command>
```
