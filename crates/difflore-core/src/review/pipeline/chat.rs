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

/// Probe whether `command` is locatable as an executable. A pre-flight
/// check before falling back to an agent CLI: lets us fail with an
/// actionable error ("install one of the supported CLIs, or run
/// `difflore providers setup`") instead of a cryptic OS-level "program
/// not found" emerging mid-review.
///
/// Absolute paths short-circuit on `Path::exists`. Bare names go through
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
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Pick the HTTP provider when active, otherwise fall back to whichever
/// supported agent CLI is installed locally. Detection order — Claude
/// Code, Codex, Gemini, OpenCode — matches the priority of "what most
/// difflore users have lying around" and gives Claude users a zero-config
/// experience. `gate4agent` owns the actual spawn.
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
            // First-run failure mode: no HTTP provider AND no agent CLI
            // available. Spell out exactly how to get out of it.
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

/// Dispatch the main review LLM call across engines. HTTP path goes through
/// the segmented provider call (preserving Anthropic `cache_control`);
/// agent-CLI path flattens the segmented prompt and sends it through
/// `gate4agent`.
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
                // Match the default `providers setup` writes for Claude
                // providers. Newer Sonnet rev gives better recall; pricing
                // is identical.
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

/// Run a single perspective pass against the AI provider and parse the
/// response into issues. All shared state is passed by reference so the
/// caller can prepare it exactly once and fan out multiple perspectives
/// concurrently.
///
/// A failed provider call or unparseable response is logged and returns an
/// empty `Vec`, preserving the "one bad perspective shouldn't kill the
/// whole review" fault tolerance.
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
            eprintln!(
                "[review_check_multi] perspective {} failed: {:?}",
                run.perspective.name(),
                e
            );
            Vec::new()
        }
    }
}
