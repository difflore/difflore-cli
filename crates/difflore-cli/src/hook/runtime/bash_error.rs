use crate::hook::{adapters::types::HookResult, forward};
use difflore_core::observability::injection_log::InjectionDropReason;

#[derive(Debug, Clone, PartialEq, Eq)]
struct BashErrorSignal {
    command: Option<String>,
    first_error: String,
    file: Option<String>,
}

pub(super) async fn recall_for_bash_error(
    hot_state: Option<&forward::State>,
    diff: Option<&str>,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> anyhow::Result<HookResult> {
    let Some(signal) = detect_bash_error(diff.unwrap_or_default()) else {
        return Ok(HookResult::noop_with_reason(
            InjectionDropReason::NotApplicable,
        ));
    };
    let target_files = signal.file.iter().cloned().collect::<Vec<_>>();
    let mut project_ctx = super::project::resolve_hook_project_context(cwd, &target_files).await;
    if difflore_core::infra::env::trace_hook() {
        eprintln!(
            "[difflore.hook.trace] resolved bash project reason={} repo_root={} project_hash={} repo_scopes={}",
            project_ctx.reason,
            project_ctx
                .repo_root
                .as_deref()
                .map_or_else(|| "<none>".to_owned(), |p| p.display().to_string()),
            project_ctx.project_hash.as_deref().unwrap_or("<none>"),
            project_ctx.repo_scopes.join(",")
        );
    }

    let db = if let Some(state) = hot_state {
        state.db.clone()
    } else {
        match difflore_core::infra::db::init_db().await {
            Ok(p) => p,
            Err(_) => {
                return Ok(HookResult::noop_with_reason(
                    InjectionDropReason::DbUnavailable,
                ));
            }
        }
    };
    super::project::refresh_repo_scopes(Some(&db), &mut project_ctx).await;
    let Ok(index_pool) = super::project::index_pool_for_project_context(
        hot_state,
        project_ctx.project_hash.as_deref(),
    )
    .await
    else {
        return Ok(HookResult::noop_with_reason(
            InjectionDropReason::IndexUnavailable,
        ));
    };

    let file = if signal.file.is_some() {
        project_ctx.recall_file.as_str()
    } else {
        "unknown"
    };
    let intent = signal.retrieval_intent();
    match difflore_core::mcp_server::fetch_relevant_rules_for_bash_error_with_repo_scopes(
        &db,
        &index_pool,
        file,
        &intent,
        session_id,
        &project_ctx.repo_scopes,
    )
    .await
    {
        Ok(ctx) if ctx.rules_injected > 0 => {
            let mut result = HookResult::with_context(ctx.rendered);
            result.rules_injected = Some(ctx.rules_injected);
            Ok(result)
        }
        Ok(ctx) => Ok(HookResult::noop_with_reason(
            ctx.drop_reason
                .unwrap_or(InjectionDropReason::RetrievalEmpty),
        )),
        Err(_) => Ok(HookResult::noop_with_reason(
            InjectionDropReason::RetrievalError,
        )),
    }
}

impl BashErrorSignal {
    fn retrieval_intent(&self) -> String {
        let command = self.command.as_deref().unwrap_or("unknown command");
        format!(
            "bash-error command={} error={}",
            truncate_for_query(command),
            truncate_for_query(&self.first_error)
        )
    }
}

fn detect_bash_error(diff: &str) -> Option<BashErrorSignal> {
    let trimmed = diff.trim();
    if trimmed.len() < difflore_core::hook_signal::BASH_MIN_ERROR_OUTPUT_CHARS {
        return None;
    }

    let command = extract_shell_command(trimmed);
    if command.as_deref().is_some_and(is_ignored_git_command) {
        return None;
    }

    let output = extract_shell_output(trimmed);
    if output.len() < difflore_core::hook_signal::BASH_MIN_ERROR_OUTPUT_CHARS
        || !difflore_core::hook_signal::bash_output_is_high_signal_failure(&output)
    {
        return None;
    }

    let first_error = first_meaningful_error_line(&output)?;
    let file = first_path_like_token(&output);
    Some(BashErrorSignal {
        command,
        first_error,
        file,
    })
}

fn extract_shell_command(diff: &str) -> Option<String> {
    diff.lines()
        .find_map(|line| line.strip_prefix("$ ").map(str::trim))
        .filter(|command| !command.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_shell_output(diff: &str) -> String {
    let mut out = String::new();
    for line in diff.lines() {
        if let Some(output_line) = line.strip_prefix('+') {
            out.push_str(output_line);
            out.push('\n');
        }
    }
    out
}

fn is_ignored_git_command(command: &str) -> bool {
    let lower = command.trim().to_ascii_lowercase();
    lower.starts_with("git commit")
        || lower.starts_with("git merge")
        || lower.starts_with("git rebase")
}

fn first_meaningful_error_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| difflore_core::hook_signal::bash_line_is_meaningful_error(line))
        .map(ToOwned::to_owned)
}

fn first_path_like_token(output: &str) -> Option<String> {
    for token in output.split_whitespace() {
        let cleaned = token
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                )
            })
            .split(':')
            .next()
            .unwrap_or_default();
        if is_path_like(cleaned) {
            return Some(cleaned.to_owned());
        }
    }
    None
}

fn is_path_like(value: &str) -> bool {
    if value.starts_with("http://") || value.starts_with("https://") {
        return false;
    }
    let has_separator = value.contains('/') || value.contains('\\');
    let has_known_extension = [
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".kt", ".rb", ".php", ".swift",
        ".c", ".cc", ".cpp", ".h", ".hpp",
    ]
    .iter()
    .any(|ext| value.ends_with(ext));
    has_known_extension && (has_separator || !value.chars().any(char::is_whitespace))
}

fn truncate_for_query(value: &str) -> String {
    const LIMIT: usize = 160;
    let trimmed = value.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_owned();
    }
    trimmed.chars().take(LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_python_traceback_with_file() {
        let diff = "$ pytest tests/foo_test.py\n+Traceback (most recent call last):\n+  File \"src/app/foo.py\", line 9, in run\n+ValueError: invalid state for typed parser that expects a longer message\n";
        let signal = detect_bash_error(diff).expect("traceback should be high-signal");
        assert_eq!(signal.command.as_deref(), Some("pytest tests/foo_test.py"));
        assert_eq!(signal.file.as_deref(), Some("src/app/foo.py"));
        assert!(signal.first_error.contains("Traceback"));
    }

    #[test]
    fn ignores_short_or_benign_output() {
        assert!(detect_bash_error("$ echo ok\n+ok\n").is_none());
        assert!(detect_bash_error("$ cargo test\n+test result: ok. 12 passed\n").is_none());
    }

    #[test]
    fn skips_git_history_commands() {
        let diff = "$ git rebase main\n+Error: conflict in src/app/foo.rs\n+Exception: manual conflict needs resolution and should not trigger recall\n";
        assert!(detect_bash_error(diff).is_none());
    }

    #[test]
    fn detects_rust_compile_error() {
        let diff = "$ cargo test\n+error[E0308]: mismatched types\n+  --> crates/app/src/lib.rs:12:9\n+expected String, found &str in a realistic compiler diagnostic\n";
        let signal = detect_bash_error(diff).expect("rust error should be high-signal");
        assert_eq!(signal.file.as_deref(), Some("crates/app/src/lib.rs"));
    }
}
