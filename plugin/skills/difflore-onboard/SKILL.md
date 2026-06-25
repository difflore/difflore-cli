---
name: difflore-onboard
description: Guide a user through first local difflore value in a repo: init, import PR review memory locally, preview recall, and report receipts after each step.
---

# DiffLore Onboard

Use this when the user wants to start using DiffLore in a repo, verify that it is wired, or get from a cold checkout to the first useful recall.

## Flow

1. Confirm you are in the intended git repo.
2. Run `difflore init`.
3. Run `difflore import-reviews --dry-run`.
4. If the dry run is healthy, run `difflore import-reviews`.
5. Run `difflore recall --diff`.
6. End with a concrete `difflore status` receipt. Only call it value when accepted edits were actually captured.

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

Keep local memory first. Cloud login, upload, and team sync are upgrades:

- Use `difflore cloud login` only when the user asks for team sync or multi-device memory.
- Use `difflore import-reviews --upload` only after the user has opted into cloud processing.
- Existing local conversation captures and imported candidates stay local unless explicitly synced.
