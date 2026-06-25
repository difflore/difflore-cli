use std::path::{Path, PathBuf};

use crate::cli::LearnCliArgs;
use crate::runtime::CommandContext;
use crate::style::{self, sym};
use crate::support::util::exit_code;

const DEFAULT_MAX_PAIRS: usize = 10;
const MANUAL_NOTE_ASSISTANT_TEXT: &str = "(manual learning note supplied by user)";

pub(crate) async fn handle_learn(ctx: &CommandContext, args: LearnCliArgs) {
    let transcript = args
        .transcript
        .or_else(|| latest_claude_transcript(&ctx.project));
    let transcript_json = transcript
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let session_id = args
        .session
        .or_else(|| transcript.as_deref().and_then(session_id_from_transcript))
        .unwrap_or_else(|| "manual-learn".to_owned());

    let mut pairs = transcript
        .as_deref()
        .map(|path| extract_pairs(&args.client, path))
        .unwrap_or_default();

    let note = args
        .note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(note) = note {
        pairs.push(crate::session_mine::extract::Pair {
            user_prompt: note.to_owned(),
            assistant_text: MANUAL_NOTE_ASSISTANT_TEXT.to_owned(),
        });
    }

    if pairs.is_empty() {
        if args.json {
            print_json(&serde_json::json!({
                "status": "nothing",
                "sessionId": session_id,
                "transcript": transcript_json,
                "message": "No transcript pairs or note found"
            }));
        } else {
            println!(
                "{} No recent session transcript or note found.",
                style::warn(sym::WARN)
            );
            println!(
                "  {} Pass {} or {}.",
                style::pewter(sym::BULLET),
                style::cmd("difflore learn --note \"...\""),
                style::cmd("difflore learn --transcript <path>")
            );
        }
        return;
    }

    let cwd = ctx.project.to_string_lossy().to_string();
    let result = crate::session_mine::run_targeted_pairs_once(
        &args.client,
        pairs,
        Some(&session_id),
        Some(&cwd),
        crate::session_mine::GateMode::ManualLearn,
    )
    .await;

    match result {
        Ok(()) => {
            if args.json {
                print_json(&serde_json::json!({
                    "status": "ok",
                    "sessionId": session_id,
                    "transcript": transcript_json,
                }));
            } else {
                println!(
                    "{} DiffLore ran learning extraction for session `{}`.",
                    style::ok(sym::OK),
                    session_id
                );
                println!(
                    "  {} Candidates still require normal review/approval.",
                    style::pewter(sym::BULLET)
                );
            }
        }
        Err(e) => {
            if args.json {
                print_json(&serde_json::json!({
                    "status": "error",
                    "sessionId": session_id,
                    "transcript": transcript_json,
                    "error": e
                }));
            } else {
                eprintln!("{} learning extraction failed: {e}", style::err(sym::ERR));
            }
            exit_code(1);
        }
    }
}

fn extract_pairs(client: &str, transcript_path: &Path) -> Vec<crate::session_mine::extract::Pair> {
    let transcript = transcript_path.to_string_lossy();
    crate::session_mine::extract::extract_recent_session_pairs(
        crate::session_mine::extract::ExtractArgs {
            platform: crate::session_mine::extract::Platform::from_client_name(client),
            transcript_path: Some(&transcript),
            session_id: None,
            max_pairs: DEFAULT_MAX_PAIRS,
        },
    )
    .unwrap_or_default()
}

fn latest_claude_transcript(repo_root: &Path) -> Option<PathBuf> {
    let home = claude_home_dir()?;
    latest_claude_transcript_under_home(&home, repo_root)
}

fn latest_claude_transcript_under_home(home: &Path, repo_root: &Path) -> Option<PathBuf> {
    let slug = claude_project_slug(repo_root)?;
    let root = home.join(".claude").join("projects").join(slug);
    newest_jsonl_under(&root)
}

fn claude_home_dir() -> Option<PathBuf> {
    if let Some(home) =
        difflore_core::infra::env::var_os(difflore_core::infra::env::DIFFLORE_CLAUDE_HOME)
        && !home.is_empty()
    {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir()
}

fn claude_project_slug(repo_root: &Path) -> Option<String> {
    let canonical = repo_root.canonicalize().ok()?;
    Some(claude_project_slug_from_path_text(
        &canonical.to_string_lossy(),
    ))
}

fn claude_project_slug_from_path_text(path: &str) -> String {
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    path.chars()
        .map(|ch| match ch {
            '\\' | '/' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '-',
            _ => ch,
        })
        .collect()
}

fn newest_jsonl_under(root: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    visit_jsonl(root, &mut newest);
    newest.map(|(_, path)| path)
}

fn visit_jsonl(dir: &Path, newest: &mut Option<(std::time::SystemTime, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_jsonl(&path, newest);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let replace = newest
            .as_ref()
            .is_none_or(|(current, _)| modified > *current);
        if replace {
            *newest = Some((modified, path));
        }
    }
}

fn session_id_from_transcript(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_uses_file_stem() {
        assert_eq!(
            session_id_from_transcript(Path::new("/tmp/sess-1.jsonl")).as_deref(),
            Some("sess-1")
        );
    }

    #[test]
    fn newest_jsonl_under_picks_latest_modified_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let older = dir.path().join("older.jsonl");
        let newer = dir.path().join("nested").join("newer.jsonl");
        std::fs::write(&older, "{}\n").expect("write older");
        std::fs::create_dir_all(newer.parent().expect("parent")).expect("mkdir");
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&newer, "{}\n").expect("write newer");

        assert_eq!(
            newest_jsonl_under(dir.path()).as_deref(),
            Some(newer.as_path())
        );
    }

    #[test]
    fn latest_claude_transcript_is_scoped_to_current_project_slug() {
        let home = tempfile::TempDir::new().expect("home tempdir");
        let repo = tempfile::TempDir::new().expect("repo tempdir");
        let repo_slug = claude_project_slug(repo.path()).expect("repo slug");
        let project_dir = home.path().join(".claude").join("projects").join(repo_slug);
        let other_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-other-repo");
        std::fs::create_dir_all(&project_dir).expect("project dir");
        std::fs::create_dir_all(&other_dir).expect("other dir");
        let expected = project_dir.join("current.jsonl");
        let other = other_dir.join("newer.jsonl");
        std::fs::write(&expected, "{}\n").expect("write current transcript");
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&other, "{}\n").expect("write other transcript");

        assert_eq!(
            latest_claude_transcript_under_home(home.path(), repo.path()).as_deref(),
            Some(expected.as_path()),
            "default learn transcript must not cross into another Claude project"
        );
    }
}
