---
name: session-recap
description: After editing code in a difflore repo, optionally end your final summary with one quiet line when accepted edits were captured for this task. Use when wrapping up a coding task.
---

# Session Recap

When you finish editing code, add **one line** to your wrap-up only when
difflore captured accepted edits for this task:

```bash
difflore status   # read accepted edits, not the heading
```

> difflore: 2 accepted edits captured.

## Rules

- **Numbers come from `difflore status`. Never invent them.**
- Do not prefix the line with `session-recap:` or the skill name.
- If you use a label, write lowercase `difflore:`.
- **Nothing applied or only recall/agent-ready activity? Say nothing.**
- Mention pending captures only if this task created them and the command output
  gave concrete ids; label them as pending review, not active agent behavior.
- Do not mention "top memory", "best memory", recall counts, ready-for-agent
  counts, or "no accepted edits yet" in the recap line.
- Name a source repo only if you are citing a specific memory that directly
  shaped the edit in your main summary, not as a generic recap metric.
- Do not translate accepted edits into saved time, ROI, avoided comments, or
  reduced review rework.
- One line. Once per session. Not a pitch.
