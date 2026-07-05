use crate::hook::adapters::types::HookResult;
use crate::hook::{cache, forward};
use std::path::{Path, PathBuf};

const PRE_SUBMIT_NUDGE: &str = "DiffLore pre-submit review: before committing, pushing, or opening a PR, run `difflore review --diff all`; fix real findings, then run it again. Use the full flow in `difflore://skills/pre-submit-review`. Do not commit, push, open a PR, or apply broad rewrites unless the user explicitly asks.";
const PRE_SUBMIT_RULE_HEADING: &str = "Diff-matched team rules:";
const PRE_SUBMIT_RULE_LIMIT: usize = 5;
const DIFF_INTENT_MAX_BYTES: usize = 100_000;

const ENGLISH_POSITIVE_PHRASES: &[&str] = &[
    "pre-submit",
    "pre submit",
    "before commit",
    "before committing",
    "before push",
    "before pushing",
    "before pr",
    "before opening a pr",
    "before pull request",
    "before submitting",
    "ready to commit",
    "ready to push",
    "open a pr",
    "create a pr",
    "submit code",
    "final code review",
    "final review before",
    "ship this",
    "ship it",
];

const ENGLISH_NEGATIVE_PHRASES: &[&str] = &[
    "don't commit",
    "dont commit",
    "do not commit",
    "don't push",
    "dont push",
    "do not push",
    "don't open a pr",
    "do not open a pr",
];

const CHINESE_POSITIVE_PHRASES: &[&str] = &[
    "提交前",
    "提交代码前",
    "准备提交",
    "帮我提交",
    "推送前",
    "推送代码",
    "发pr",
    "发 pr",
    "提pr",
    "提 pr",
    "开pr",
    "开 pr",
    "pr前",
    "pr 前",
    "合并前",
    "最终检查",
    "最后检查一下",
    "发布前检查",
];

const CHINESE_NEGATIVE_PHRASES: &[&str] = &[
    "不要提交",
    "先别提交",
    "不用提交",
    "不要推送",
    "先别推送",
    "不用推送",
];

pub(super) fn nudge_for_prompt(prompt: &str) -> Option<HookResult> {
    build_nudge_for_prompt(prompt, None)
}

pub(super) async fn nudge_for_prompt_with_diff_rules(
    hot_state: Option<&forward::State>,
    prompt: &str,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Option<HookResult> {
    if !has_pre_submit_intent(prompt) {
        return None;
    }
    let Some(ctx) = recall_compact_rules_for_current_diff(hot_state, session_id, cwd).await else {
        return nudge_for_prompt(prompt);
    };
    if ctx.rules_injected == 0 || ctx.rendered.trim().is_empty() {
        return nudge_for_prompt(prompt);
    }
    build_nudge_for_prompt(prompt, Some(&ctx.rendered))
}

fn has_pre_submit_intent(prompt: &str) -> bool {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return false;
    }

    let lower = prompt.to_ascii_lowercase();
    if contains_any(&lower, ENGLISH_NEGATIVE_PHRASES)
        || contains_any(prompt, CHINESE_NEGATIVE_PHRASES)
    {
        return false;
    }

    contains_any(&lower, ENGLISH_POSITIVE_PHRASES) || contains_any(prompt, CHINESE_POSITIVE_PHRASES)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn build_nudge_for_prompt(prompt: &str, compact_rule_lines: Option<&str>) -> Option<HookResult> {
    if !has_pre_submit_intent(prompt) {
        return None;
    }
    let Some(rule_lines) = compact_rule_lines.and_then(compact_rule_lines_block) else {
        return Some(HookResult::with_context(PRE_SUBMIT_NUDGE));
    };
    let rules_injected = rule_lines.lines().count();
    let mut result = HookResult::with_context(format!(
        "{PRE_SUBMIT_NUDGE}\n\n{PRE_SUBMIT_RULE_HEADING}\n{rule_lines}"
    ));
    result.rules_injected = Some(rules_injected);
    Some(result)
}

fn compact_rule_lines_block(lines: &str) -> Option<String> {
    let lines = lines
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(PRE_SUBMIT_RULE_LIMIT)
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

async fn recall_compact_rules_for_current_diff(
    hot_state: Option<&forward::State>,
    session_id: Option<&str>,
    cwd: Option<&str>,
) -> Option<difflore_core::mcp_server::HookRuleContext> {
    let snapshot = current_diff_snapshot(cwd).await?;
    let primary_file = snapshot.files.first()?.clone();
    let diff_intent = difflore_core::context::intent_filter::build_review_intent_text(
        Some(&primary_file),
        &snapshot.diff_text,
    );
    let intent = if diff_intent.trim().is_empty() {
        format!(
            "pre-submit review for current diff: {}",
            snapshot.files.join(", ")
        )
    } else {
        diff_intent
    };

    let mut project_ctx = super::project::resolve_hook_project_context(
        Some(&path_to_string(&snapshot.repo_root)),
        &snapshot.files,
    )
    .await;
    let cache_project_hash = project_ctx.project_hash.clone();
    let cache_file_key = snapshot.files.join(",");
    let cache_signal = format!("{intent}\n{}", snapshot.diff_text);
    let should_skip = if let Some(hash) = cache_project_hash.as_deref() {
        cache::should_skip_recent_lookup_for_project_hash_with_signal(
            &cache_file_key,
            "pre-submit",
            hash,
            Some(&cache_signal),
        )
    } else {
        cache::should_skip_recent_lookup_with_signal(
            &cache_file_key,
            "pre-submit",
            Some(&cache_signal),
        )
    };
    if should_skip {
        return None;
    }

    let db = if let Some(state) = hot_state {
        state.db.clone()
    } else {
        difflore_core::infra::db::init_db().await.ok()?
    };
    super::project::refresh_repo_scopes(Some(&db), &mut project_ctx).await;
    if project_ctx.repo_scopes.is_empty() {
        return None;
    }
    let index_pool = super::project::index_pool_for_project_context(
        hot_state,
        project_ctx.project_hash.as_deref(),
    )
    .await
    .ok()?;

    let ctx =
        difflore_core::mcp_server::fetch_compact_relevant_rules_for_pre_submit_with_repo_scopes(
            &db,
            &index_pool,
            &primary_file,
            &snapshot.files,
            &intent,
            session_id,
            &project_ctx.repo_scopes,
        )
        .await
        .ok()?;
    if let Some(hash) = cache_project_hash.as_deref() {
        cache::remember_injection_for_project_hash_with_signal(
            &cache_file_key,
            "pre-submit",
            ctx.rules_injected,
            hash,
            Some(&cache_signal),
        );
    } else {
        cache::remember_injection(
            &cache_file_key,
            "pre-submit",
            ctx.rules_injected,
            Some(&cache_signal),
        );
    }
    Some(ctx)
}

#[derive(Debug)]
struct DiffSnapshot {
    repo_root: PathBuf,
    files: Vec<String>,
    diff_text: String,
}

async fn current_diff_snapshot(cwd: Option<&str>) -> Option<DiffSnapshot> {
    let cwd = cwd
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())?;
    tokio::task::spawn_blocking(move || current_diff_snapshot_blocking(&cwd))
        .await
        .ok()
        .flatten()
}

fn current_diff_snapshot_blocking(cwd: &Path) -> Option<DiffSnapshot> {
    let repo_root = git_repo_root(cwd)?;
    let files = git_diff_files(&repo_root)?;
    if files.is_empty() {
        return None;
    }
    let diff_text = truncate_bytes_lossy(&git_diff_text(&repo_root)?, DIFF_INTENT_MAX_BYTES);
    Some(DiffSnapshot {
        repo_root,
        files,
        diff_text,
    })
}

fn git_repo_root(cwd: &Path) -> Option<PathBuf> {
    let output = difflore_core::infra::git::git_command(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

fn git_diff_files(repo_root: &Path) -> Option<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    let mut files = Vec::new();
    for args in [
        &["diff", "--name-only"][..],
        &["diff", "--name-only", "--cached"][..],
    ] {
        let output = difflore_core::infra::git::git_command(repo_root)
            .args(args)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let file = line.trim();
            if !file.is_empty() && seen.insert(file.to_owned()) {
                files.push(file.to_owned());
            }
        }
    }
    Some(files)
}

fn git_diff_text(repo_root: &Path) -> Option<String> {
    let mut out = String::new();
    for args in [
        &["diff", "--no-ext-diff", "--unified=8"][..],
        &["diff", "--cached", "--no-ext-diff", "--unified=8"][..],
    ] {
        let output = difflore_core::infra::git::git_command(repo_root)
            .args(args)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        out.push_str(&String::from_utf8_lossy(&output.stdout));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Some(out)
}

fn truncate_bytes_lossy(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_owned()
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::adapters::PlatformAdapter;

    #[test]
    fn detects_english_pre_submit_intent() {
        let result = nudge_for_prompt("Before committing, can you do the final code review?")
            .expect("pre-submit wording should nudge");

        let ctx = result.additional_context.expect("nudge context");
        assert!(ctx.contains("difflore review --diff all"));
        assert!(ctx.contains("difflore://skills/pre-submit-review"));
        assert!(ctx.contains("Do not commit"));
    }

    #[test]
    fn intent_with_rule_lines_appends_compact_rules() {
        let result = build_nudge_for_prompt(
            "ready to push; please do a final review",
            Some("- Avoid unwrap in handlers ← PR #241 · src/**/*.rs\n- Keep migrations paired ← PR #242 · migrations/**"),
        )
        .expect("pre-submit wording should nudge");

        let ctx = result.additional_context.expect("nudge context");
        assert!(ctx.contains("difflore review --diff all"));
        assert!(ctx.contains(PRE_SUBMIT_RULE_HEADING));
        assert!(ctx.contains("- Avoid unwrap in handlers ← PR #241 · src/**/*.rs"));
        assert!(ctx.contains("- Keep migrations paired ← PR #242 · migrations/**"));
        assert_eq!(result.rules_injected, Some(2));
    }

    #[test]
    fn intent_without_rule_lines_keeps_nudge_only() {
        let result = build_nudge_for_prompt("ship it after a final review", Some(""))
            .expect("pre-submit wording should nudge");

        let ctx = result.additional_context.expect("nudge context");
        assert!(ctx.contains("difflore review --diff all"));
        assert!(!ctx.contains(PRE_SUBMIT_RULE_HEADING));
        assert_eq!(result.rules_injected, None);
    }

    #[test]
    fn no_intent_with_rule_lines_stays_noop() {
        assert!(
            build_nudge_for_prompt("Can you explain this diff?", Some("- Rule ← PR #1")).is_none()
        );
    }

    #[test]
    fn compact_rule_lines_are_capped_at_five() {
        let lines = (1..=6)
            .map(|n| format!("- Rule {n} ← PR #{n} · src/**"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = build_nudge_for_prompt("ready to commit", Some(&lines))
            .expect("pre-submit wording should nudge");

        let ctx = result.additional_context.expect("nudge context");
        let rule_line_count = ctx
            .lines()
            .filter(|line| line.starts_with("- Rule "))
            .count();
        assert_eq!(rule_line_count, 5);
        assert!(ctx.contains("- Rule 5 ← PR #5 · src/**"));
        assert!(!ctx.contains("- Rule 6 ← PR #6 · src/**"));
        assert_eq!(result.rules_injected, Some(5));
    }

    #[test]
    fn detects_chinese_pre_submit_intent() {
        assert!(nudge_for_prompt("提交前帮我最后检查一下").is_some());
        assert!(nudge_for_prompt("准备提交到远程了，先检查").is_some());
    }

    #[test]
    fn broad_git_or_negated_mentions_stay_noop() {
        assert!(nudge_for_prompt("Can you explain git commit message style?").is_none());
        assert!(nudge_for_prompt("Do not commit yet; just inspect the status.").is_none());
        assert!(nudge_for_prompt("先别提交，看看 diff。").is_none());
    }

    #[test]
    fn claude_output_surfaces_pre_submit_nudge_as_context() {
        let mut result =
            nudge_for_prompt("ready to push; please do a final review").expect("should nudge");
        result.event_name = Some("UserPromptSubmit".to_owned());

        let adapter = crate::hook::adapters::claude_code::ClaudeCodeAdapter;
        let out = adapter.format_output(result);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid json");

        assert_eq!(value["continue"], true);
        assert_eq!(
            value["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert!(
            value["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .expect("additional context")
                .contains("pre-submit-review")
        );
    }
}
