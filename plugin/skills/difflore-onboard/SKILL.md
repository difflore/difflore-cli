---
name: difflore-onboard
description: Guide a user through first local difflore value in a repo: init, import private PR review backlog locally, wire local AI CLIs, preview recall, and report receipts after each step.
---

# DiffLore Onboard

Use this when the user wants to start using DiffLore in a private or public repo, verify that local AI CLI wiring works, or get from a cold checkout to the first useful recall from their own review backlog.

## Flow

1. Confirm you are in the intended git repo.
2. Run `difflore init`.
3. Confirm `difflore agents install` was run or that `difflore init` wired the detected local AI CLIs.
4. Run `difflore import-reviews --dry-run`.
5. If the dry run is healthy, run `difflore import-reviews`.
6. If drafts were created, run `difflore memory review` before calling them active rules.
7. Run `difflore recall --diff`.
8. End with a concrete `difflore status` receipt. Only call it value when accepted edits were actually captured.

## Receipts

After every write step, echo the concrete receipt line DiffLore printed, such as:

- `+N local memory writes`
- `+1 rule captured from agent chat`
- `+N accepted edits recorded for local value tracking`

If a command writes nothing, say what the next command is and do not invent value numbers.
If a command creates pending candidates, say they were saved for review and are
not active rules until approved. Never report "N learnings" unless the command
printed that number.

## Upgrade Path

Keep local private review backlog import first. Cloud login, upload, and team sync are upgrades:

- Use `difflore cloud login` only when the user asks for team sync, multi-device access, managed tokens, or managed embeddings.
- Use `difflore import-reviews --upload` only after the user has opted into cloud processing.
- Existing local conversation captures and imported candidates stay local unless explicitly synced.
