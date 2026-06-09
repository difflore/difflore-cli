use std::path::PathBuf;

use difflore_core::stated_vs_actual;

/// Read at most the last `MAX_TRANSCRIPT_BYTES` of a hook-controlled transcript.
/// The scans only need recent turns at the tail; a partial first line after the
/// seek boundary is skipped by the per-line parser.
const MAX_TRANSCRIPT_BYTES: u64 = 32 * 1024 * 1024;

fn read_transcript_tail_capped(path: &str) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > MAX_TRANSCRIPT_BYTES {
        file.seek(SeekFrom::Start(len - MAX_TRANSCRIPT_BYTES))
            .ok()?;
    }
    let mut buf = Vec::new();
    file.take(MAX_TRANSCRIPT_BYTES).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Compare the agent's last assistant message against the actual
/// `git diff --name-only` in `cwd`. Returns a short user-visible warning
/// when the agent claimed to edit files that aren't in the diff. Returns
/// `None` on any error (missing transcript, JSONL parse failure, git not
/// available, no mismatch found). Designed to be a strictly advisory
/// audit step — must never block a hook.
pub(super) fn stated_vs_actual_warning(transcript_path: &str, cwd: &str) -> Option<String> {
    let claim_text = read_last_assistant_text(transcript_path)?;
    if claim_text.trim().is_empty() {
        return None;
    }
    // Short-circuit zero-edit turns. The validator's purpose is catching
    // "agent claimed to edit X, didn't" — but every assistant reply that
    // merely *mentions* a filename in prose (status reports, error
    // diagnostics, commit-message drafts, citations) gets flagged the
    // same way. Skip the prose comparison entirely when the assistant
    // didn't fire any edit-class tool in the most recent turn — those
    // rows can't have produced or claimed an edit anyway.
    if !last_assistant_turn_invoked_edit_tool(transcript_path) {
        return None;
    }
    let actual = git_changed_files(cwd)?;
    let expected: Vec<PathBuf> = Vec::new(); // hint not available at hook time
    let finding = stated_vs_actual::validate(&claim_text, &actual, &expected)?;
    Some(format!("⚠ DiffLore: {}", finding.summary_for_user()?))
}

/// True if any `tool_use` entry in the most recent assistant turn (i.e.
/// since the last user message in the transcript) names an edit-class
/// tool. Conservative — unknown tools and parse failures count as
/// "no edit", so a malformed transcript silently suppresses the
/// warning rather than mis-firing it.
///
/// Recognised edit-class tools mirror the Claude Code surface:
///   - `Edit` / `MultiEdit` / `Write` / `NotebookEdit`: direct file mutations
///   - `Bash`: only when the command name matches a writing shell verb
///     (`echo >`, `tee`, `cp`, `mv`, `sed -i`, `git apply`, `git commit -m`,
///     etc.). We use a coarse keyword list rather than a real shell parser
///     because the goal is "did the agent likely write something to disk",
///     not "what exactly did the command do".
fn last_assistant_turn_invoked_edit_tool(transcript_path: &str) -> bool {
    const EDIT_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];
    const BASH_WRITE_KEYWORDS: &[&str] = &[
        " > ",
        ">>",
        " tee ",
        "tee ",
        " cp ",
        "cp ",
        " mv ",
        "mv ",
        "sed -i",
        "git apply",
        "git commit",
        "git add",
    ];
    let Some(body) = read_transcript_tail_capped(transcript_path) else {
        return false;
    };
    let mut found_edit = false;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // Reset the window each time we see a user row — only the
        // most recent assistant turn (since last user) counts.
        let role = v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .or_else(|| v.get("type").and_then(|t| t.as_str()));
        if role == Some("user") {
            found_edit = false;
            continue;
        }
        if role != Some("assistant") {
            continue;
        }
        let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let Some(arr) = content.as_array() else {
            continue;
        };
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = part.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if EDIT_TOOLS.contains(&name) {
                found_edit = true;
                break;
            }
            if name == "Bash" {
                let cmd = part
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                if BASH_WRITE_KEYWORDS.iter().any(|kw| cmd.contains(kw)) {
                    found_edit = true;
                    break;
                }
            }
        }
    }
    found_edit
}

/// Read the last assistant message's concatenated text content from a
/// Claude-Code-style session JSONL. Each line is one JSON object with a
/// `type` field (`"user"` / `"assistant"` / `"system"` / …) and a
/// `message` object whose `content` is an array of typed parts. We only
/// keep parts where `type == "text"` and concatenate their `text` values.
pub(super) fn read_last_assistant_text(transcript_path: &str) -> Option<String> {
    let body = read_transcript_tail_capped(transcript_path)?;
    let mut last_text: Option<String> = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // skip malformed lines, keep walking
        };
        // Claude Code's session JSONL marks the row type two ways
        // depending on version: a top-level `"type":"assistant"` and/or
        // a nested `"message":{"role":"assistant"}`. Accept either.
        let is_assistant = v.get("type").and_then(|t| t.as_str()) == Some("assistant")
            || v.get("message")
                .and_then(|m| m.get("role"))
                .and_then(|r| r.as_str())
                == Some("assistant");
        if !is_assistant {
            continue;
        }
        let content = v.get("message").and_then(|m| m.get("content"))?;
        let mut buf = String::new();
        if let Some(arr) = content.as_array() {
            for part in arr {
                if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(text);
                    }
                }
            }
        } else if let Some(s) = content.as_str() {
            buf.push_str(s);
        }
        if !buf.is_empty() {
            last_text = Some(buf);
        }
    }
    last_text
}

/// Run `git diff --name-only HEAD` in `cwd` and return the changed paths
/// plus untracked-but-not-gitignored new files. Without the
/// `ls-files --others` step, the validator would flag any agent-created
/// new file as "hallucinated" because `diff --name-only HEAD` only sees
/// tracked-file modifications. Untracked-but-listed-in-gitignore files
/// are excluded so agent claims about generated artefacts (e.g. `dist/`,
/// `.output/`) don't fire either.
fn git_changed_files(cwd: &str) -> Option<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = Vec::new();

    // Modified tracked files.
    let modified = crate::commands::util::git_str_in(cwd, &["diff", "--name-only", "HEAD"])?;
    for line in modified.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            paths.push(PathBuf::from(trimmed));
        }
    }

    // Untracked-but-not-gitignored files. Run separately because git
    // doesn't combine "modified tracked" + "new untracked" into one
    // command; failures here are non-fatal — better to under-report
    // (some new-file false positives) than refuse the whole audit.
    if let Some(untracked) =
        crate::commands::util::git_str_in(cwd, &["ls-files", "--others", "--exclude-standard"])
    {
        for line in untracked.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                paths.push(PathBuf::from(trimmed));
            }
        }
    }

    Some(paths)
}

#[cfg(test)]
mod stated_vs_actual_tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".jsonl")
            .tempfile()
            .expect("tempfile");
        for line in lines {
            writeln!(f, "{line}").expect("write");
        }
        f
    }

    #[test]
    fn read_last_assistant_text_picks_latest_assistant_row() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first reply"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"again"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"final reply"}]}}"#,
        ]);
        let got = read_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(got.as_deref(), Some("final reply"));
    }

    #[test]
    fn read_last_assistant_text_ignores_malformed_lines() {
        let f = write_jsonl(&[
            "not valid json {",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"survives"}]}}"#,
        ]);
        let got = read_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(got.as_deref(), Some("survives"));
    }

    #[test]
    fn read_last_assistant_text_concatenates_text_parts() {
        let f = write_jsonl(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"part one"},{"type":"tool_use","id":"x"},{"type":"text","text":"part two"}]}}"#,
        ]);
        let got = read_last_assistant_text(f.path().to_str().unwrap());
        assert_eq!(got.as_deref(), Some("part one\npart two"));
    }

    #[test]
    fn warning_is_none_when_no_assistant_text_in_transcript() {
        let f = write_jsonl(&[r#"{"type":"user","message":{"role":"user","content":"x"}}"#]);
        let got = stated_vs_actual_warning(f.path().to_str().unwrap(), ".");
        assert!(got.is_none());
    }

    #[test]
    fn turn_with_no_edit_tool_returns_false() {
        // Status report: assistant only emitted text, citing a filename.
        // Must NOT be treated as having claimed an edit.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"status?"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I touched activity_stream.rs earlier"}]}}"#,
        ]);
        assert!(!last_assistant_turn_invoked_edit_tool(
            f.path().to_str().unwrap()
        ));
    }

    #[test]
    fn turn_with_edit_tool_returns_true() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/foo.rs"}},{"type":"text","text":"done"}]}}"#,
        ]);
        assert!(last_assistant_turn_invoked_edit_tool(
            f.path().to_str().unwrap()
        ));
    }

    #[test]
    fn turn_with_write_tool_returns_true() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"src/new.rs","content":"…"}}]}}"#,
        ]);
        assert!(last_assistant_turn_invoked_edit_tool(
            f.path().to_str().unwrap()
        ));
    }

    #[test]
    fn bash_redirect_counts_as_edit() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"echo hi > foo.txt"}}]}}"#,
        ]);
        assert!(last_assistant_turn_invoked_edit_tool(
            f.path().to_str().unwrap()
        ));
    }

    #[test]
    fn bash_read_only_does_not_count() {
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cat foo.txt"}}]}}"#,
        ]);
        assert!(!last_assistant_turn_invoked_edit_tool(
            f.path().to_str().unwrap()
        ));
    }

    #[test]
    fn earlier_turns_dont_carry_into_current() {
        // An edit two turns ago shouldn't make the current pure-text
        // status report look like an edit claim.
        let f = write_jsonl(&[
            r#"{"type":"user","message":{"role":"user","content":"do edit"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/foo.rs"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":"now status?"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"earlier I edited foo.rs"}]}}"#,
        ]);
        assert!(!last_assistant_turn_invoked_edit_tool(
            f.path().to_str().unwrap()
        ));
    }
}
