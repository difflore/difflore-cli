---
name: pre-submit-review
description: Use when a user is about to commit, push, open a PR, submit code, or asks for a final local review after editing in a DiffLore repo.
---

# Pre-Submit Review

Before code leaves the working tree, run a local DiffLore review pass and fix
only the issues it finds. This is a delivery gate, not a commit command.

## Flow

1. Inspect the current diff: `git diff --stat` and, if useful, `git status --short`.
2. Run `difflore review --diff all`.
3. If the review reports findings, summarize them and ask before applying broad changes.
4. Fix locally with the current agent, or run `difflore fix` when interactive patching is appropriate.
5. Run `difflore review --diff all` again until it is clean or remaining items are explicitly deferred.
6. Show the user the final status and tell them to review `git diff`.

## Guardrails

- Do not commit, push, open a PR, or post review comments.
- Do not run `difflore fix --yes` unless the user explicitly asked for automatic fixes.
- Do not treat "could not review" as clean; resolve review provider/config issues or report the blocker.
- Keep fixes scoped to DiffLore findings and the user's requested change.

## Useful Commands

```bash
difflore review --diff all
difflore fix
difflore status
```

When nothing is found, say the DiffLore pre-submit review is clean and mention
that the user should still review the final diff before committing.
