use super::super::parse::parse_issues;
use super::super::prompts::{SegmentedPrompt, build_segmented_prompt};
use super::super::providers::call_ai_provider_segmented;
use super::super::{
    AgentCliReviewLlm, HttpReviewLlm, ReviewIssueRecord, ReviewLlm, ReviewPerspective,
};
use super::ReviewEngine;
use crate::context::types::PastVerdict;
use crate::errors::CoreError;
use gate4agent::CliTool;

#[derive(sqlx::FromRow)]
pub(in super::super) struct ProviderRow {
    #[allow(dead_code)]
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model_mapping: String,
    #[allow(dead_code)]
    pub is_active: i64,
}

/// Whether `command` is locatable as an executable. Pre-flight before
/// falling back to an agent CLI, so we fail with an actionable error
/// instead of a cryptic mid-review "program not found".
///
/// Absolute paths short-circuit on `Path::exists`; bare names go through
/// `where` on Windows / `which` on Unix.
pub(super) fn command_exists_on_path(command: &str) -> bool {
    let path = std::path::Path::new(command);
    if path.is_absolute() || command.contains('/') || command.contains('\\') {
        return path.exists();
    }
    let probe = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(probe)
        .arg(command)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Pick the active HTTP provider, otherwise fall back to the first
/// installed supported agent CLI. Detection order (Claude Code, Codex,
/// Gemini, OpenCode) prioritizes what most users already have, giving
/// Claude users a zero-config path. `gate4agent` owns the spawn.
pub(in super::super) async fn resolve_review_engine(
    db: &sqlx::SqlitePool,
) -> crate::Result<ReviewEngine> {
    match get_active_provider(db).await {
        Ok((provider_name, base_url, api_key, model)) => Ok(ReviewEngine::HttpProvider {
            provider_name,
            base_url,
            api_key,
            model,
        }),
        Err(CoreError::Validation(_)) => {
            for (cmd, tool) in [
                ("claude", CliTool::ClaudeCode),
                ("codex", CliTool::Codex),
                ("gemini", CliTool::Gemini),
                ("opencode", CliTool::OpenCode),
            ] {
                if command_exists_on_path(cmd) {
                    return Ok(ReviewEngine::AgentCli {
                        tool,
                        model: String::new(),
                    });
                }
            }
            // No HTTP provider and no agent CLI — spell out the way out.
            Err(CoreError::Validation(
                "no LLM provider configured and no supported agent CLI found on PATH \
                (looked for: claude, codex, gemini, opencode).\n\n  \
                Pick a provider: `difflore providers setup`\n  \
                (options: Claude Code / Codex / Gemini / OpenCode CLI, Anthropic API, \
                or any OpenAI-compatible gateway)\n\n  \
                Or install Claude Code: https://docs.anthropic.com/en/docs/claude-code"
                    .into(),
            ))
        }
        Err(e) => Err(e),
    }
}

pub(super) fn make_review_llm(engine: ReviewEngine) -> Box<dyn ReviewLlm> {
    match engine {
        ReviewEngine::HttpProvider {
            provider_name,
            base_url,
            api_key,
            model,
        } => Box::new(HttpReviewLlm {
            provider_name,
            base_url,
            api_key,
            model,
        }),
        ReviewEngine::AgentCli { tool, model } => Box::new(AgentCliReviewLlm { tool, model }),
    }
}

/// Dispatch the main review LLM call. HTTP goes through the segmented
/// provider call (preserving Anthropic `cache_control`); the agent-CLI
/// path flattens the segmented prompt through `gate4agent`.
pub(super) async fn call_review_engine(
    engine: &ReviewEngine,
    segmented: &SegmentedPrompt,
    user_prompt: &str,
) -> crate::Result<String> {
    match engine {
        ReviewEngine::HttpProvider {
            provider_name,
            base_url,
            api_key,
            model,
        } => {
            call_ai_provider_segmented(
                provider_name,
                base_url,
                api_key,
                model,
                segmented,
                user_prompt,
            )
            .await
        }
        ReviewEngine::AgentCli { tool, model } => {
            super::super::providers::call_agent_cli_provider(
                *tool,
                model,
                &segmented.stable_prefix,
                &format!("{}\n\n{}", segmented.dynamic_suffix, user_prompt),
            )
            .await
        }
    }
}

/// Get the active provider with decrypted API key.
pub(super) async fn get_active_provider(
    db: &sqlx::SqlitePool,
) -> crate::Result<(String, String, String, String)> {
    let row = sqlx::query_as!(
        ProviderRow,
        "SELECT id, name, base_url, api_key, model_mapping, is_active FROM providers WHERE is_active = 1 LIMIT 1"
    )
    .fetch_optional(db)
    .await?
    .ok_or_else(|| CoreError::Validation("No active AI provider configured. Run `difflore providers setup` to add one.".into()))?;

    let api_key = crate::crypto::decrypt_secret(&row.api_key)
        .map_err(|e| CoreError::Internal(format!("Failed to decrypt API key: {e}")))?;

    let mapping: std::collections::HashMap<String, String> =
        serde_json::from_str(&row.model_mapping).unwrap_or_default();
    let model = mapping
        .get("review")
        .or_else(|| mapping.get("default"))
        .cloned()
        .unwrap_or_else(|| {
            if row.name.to_lowercase().contains("anthropic")
                || row.name.to_lowercase().contains("claude")
            {
                // Match the default `providers setup` writes for Claude.
                "claude-sonnet-4-6".to_owned()
            } else {
                "gpt-4o".to_owned()
            }
        });

    Ok((row.name, row.base_url, api_key, model))
}

pub(super) struct PerspectiveRun<'a> {
    pub provider_name: &'a str,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub user_prompt: &'a str,
    pub perspective: ReviewPerspective,
    pub diff_content: &'a str,
    pub past_verdicts: &'a [PastVerdict],
}

/// Run a single perspective pass and parse the response into issues.
/// Shared state is passed by reference so the caller prepares it once and
/// fans out perspectives concurrently.
///
/// A failed call or unparseable response is logged and returns an empty
/// `Vec`, so one bad perspective doesn't kill the whole review.
pub(super) async fn run_one_perspective(run: PerspectiveRun<'_>) -> Vec<ReviewIssueRecord> {
    // Always build a segmented prompt so the Anthropic path can apply
    // cache_control on the stable prefix. OpenAI-compatible providers
    // concatenate the two halves into a flat system prompt.
    let seg = build_segmented_prompt(
        Some(run.perspective),
        &[],
        run.diff_content,
        "",
        None,
        if run.past_verdicts.is_empty() {
            None
        } else {
            Some(run.past_verdicts)
        },
    );
    match call_ai_provider_segmented(
        run.provider_name,
        run.base_url,
        run.api_key,
        run.model,
        &seg,
        run.user_prompt,
    )
    .await
    {
        Ok(ai_response) => {
            let parsed = parse_issues(&ai_response);
            if crate::env::fix_debug() {
                eprintln!(
                    "[fix-debug] perspective={} raw_response_len={} parsed_issues={}",
                    run.perspective.name(),
                    ai_response.len(),
                    parsed.len(),
                );
                if parsed.is_empty() && ai_response.len() < 4000 {
                    eprintln!("[fix-debug] response body: {ai_response}");
                }
            }
            parsed
        }
        Err(e) => {
            if crate::env::fix_debug() {
                eprintln!(
                    "[review_check_multi] perspective {} failed: {:?}",
                    run.perspective.name(),
                    e
                );
            }
            Vec::new()
        }
    }
}
