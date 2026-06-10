use std::time::Duration;

use super::{FixArgs, fix_debug};

pub(super) const REVIEW_TIMEOUT_SECS: u64 = 120;
pub(super) const PREVIEW_REVIEW_TIMEOUT_SECS: u64 = 15;
const MAX_REVIEW_TIMEOUT_SECS: u64 = 30 * 60;

fn supported_agent_cli_on_path() -> Option<&'static str> {
    supported_agent_cli_on_path_with(|cmd| which::which(cmd).is_ok())
}

pub(super) fn supported_agent_cli_on_path_with(
    mut exists: impl FnMut(&str) -> bool,
) -> Option<&'static str> {
    ["claude", "codex", "gemini", "opencode"]
        .into_iter()
        .find(|cmd| exists(cmd))
}

pub(super) fn no_provider_configured_message() -> String {
    "no LLM provider configured and no supported agent CLI found on PATH \
     (looked for: claude, codex, gemini, opencode).\n\n  \
     Run `difflore providers setup` to choose a provider, or install one of the supported \
     agent CLIs and retry."
        .to_owned()
}

// `fix --preview` is a trust signal: a clean preview must mean a review the
// user can vouch for actually ran. So preview requires an explicitly
// configured (`is_active`) provider and rejects the zero-config agent-CLI
// fallback, reporting `no_provider` (not_reviewed, non-zero exit) when absent.
// The keyed phrase ("no AI provider configured") must match the
// `format_fix_err` classifier so the actionable hint is preserved.
fn preview_no_provider_configured_message() -> String {
    "no AI provider configured — run `difflore providers setup`.\n\n  \
     `fix --preview` reports a real review only when a provider you configured \
     actually runs; it will not silently fall back to an agent CLI found on PATH \
     for the preview's clean/at-risk verdict.\n  \
     (The apply path, `difflore fix`, still uses an installed agent CLI when no \
     provider is configured.)"
        .to_owned()
}

/// Pure preflight decision, split from DB/PATH probing so the trust
/// contract is exhaustively unit-testable.
///
/// - An `is_active` configured provider always satisfies the check.
/// - With `require_configured_provider` (`--preview`), the agent-CLI
///   fallback is NOT accepted: no provider ⇒ `Err(preview msg)`.
/// - Otherwise (apply path) an agent CLI on PATH is acceptable; only its
///   absence is an error.
pub(super) fn preflight_decision(
    has_active_provider: bool,
    agent_cli: Option<&str>,
    require_configured_provider: bool,
) -> Result<(), String> {
    if has_active_provider {
        return Ok(());
    }
    if require_configured_provider {
        return Err(preview_no_provider_configured_message());
    }
    if let Some(cmd) = agent_cli {
        fix_debug!("using agent CLI fallback `{cmd}` for provider mode");
        return Ok(());
    }
    Err(no_provider_configured_message())
}

/// Pre-flight the review backend before running `fix`.
///
/// `require_configured_provider` is set for `--preview`: it demands an
/// `is_active` provider and rejects the agent-CLI fallback. The apply
/// path passes `false` and keeps the zero-config CLI fallback.
pub(super) async fn preflight_provider_backend(
    db: &difflore_core::SqlitePool,
    require_configured_provider: bool,
) -> Result<(), String> {
    let providers = difflore_core::providers::list(db)
        .await
        .map_err(|e| format!("failed to read provider configuration: {e}"))?;
    let has_active_provider = providers.iter().any(|provider| provider.is_active);
    // Only probe PATH when the fallback could matter.
    let agent_cli = if has_active_provider || require_configured_provider {
        None
    } else {
        supported_agent_cli_on_path()
    };
    preflight_decision(has_active_provider, agent_cli, require_configured_provider)
}

fn parse_review_timeout_override(raw: Option<&str>) -> Option<u64> {
    let value = raw?.trim().parse::<u64>().ok()?;
    (1..=MAX_REVIEW_TIMEOUT_SECS)
        .contains(&value)
        .then_some(value)
}

pub(super) fn review_timeout_for_args_with_env<'a>(
    args: &FixArgs,
    env_var: impl Fn(&'a str) -> Option<String>,
) -> Duration {
    if args.preview {
        let override_secs = env_var(difflore_core::env::DIFFLORE_FIX_PREVIEW_REVIEW_TIMEOUT_SECS)
            .and_then(|value| parse_review_timeout_override(Some(&value)));
        Duration::from_secs(override_secs.unwrap_or(PREVIEW_REVIEW_TIMEOUT_SECS))
    } else {
        Duration::from_secs(REVIEW_TIMEOUT_SECS)
    }
}

pub(super) fn review_timeout_for_args(args: &FixArgs) -> Duration {
    review_timeout_for_args_with_env(args, difflore_core::env::var)
}

pub(super) fn review_id_for_provider_run(review_id: Option<&str>, preview: bool) -> Option<String> {
    if preview {
        None
    } else {
        review_id.map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::super::FixAgentMode;
    use super::*;

    fn fix_args(preview: bool, json: bool) -> FixArgs {
        FixArgs {
            yes: false,
            preview,
            ci: false,
            strict: false,
            diff_scope: None,
            pr: None,
            repo: None,
            base: None,
            work_branch: None,
            no_checkout: false,
            allow_dirty: false,
            no_upload_acceptance: false,
            explain_rules: false,
            report: None,
            json,
            path: None,
            agent: FixAgentMode::Provider,
        }
    }

    #[test]
    fn preview_review_timeout_accepts_env_override() {
        let args = fix_args(true, true);

        assert_eq!(
            review_timeout_for_args_with_env(&args, |key| {
                (key == difflore_core::env::DIFFLORE_FIX_PREVIEW_REVIEW_TIMEOUT_SECS)
                    .then(|| "75".to_owned())
            }),
            Duration::from_secs(75)
        );
        assert_eq!(
            review_timeout_for_args_with_env(&args, |_| Some("0".to_owned())),
            Duration::from_secs(PREVIEW_REVIEW_TIMEOUT_SECS)
        );
        assert_eq!(
            review_timeout_for_args_with_env(&args, |_| Some("not-a-number".to_owned())),
            Duration::from_secs(PREVIEW_REVIEW_TIMEOUT_SECS)
        );
    }

    #[test]
    fn preview_provider_run_does_not_attach_pr_review_id() {
        assert_eq!(
            review_id_for_provider_run(Some("github-pr:owner/repo#12"), true),
            None
        );
        assert_eq!(
            review_id_for_provider_run(Some("github-pr:owner/repo#12"), false).as_deref(),
            Some("github-pr:owner/repo#12")
        );
    }

    #[test]
    fn preview_preflight_rejects_agent_cli_fallback_when_no_provider_configured() {
        // In `--preview`, an unconfigured agent CLI on PATH must NOT
        // satisfy the preflight; no configured provider ⇒ error.
        assert!(preflight_decision(false, Some("claude"), true).is_err());
        assert!(preflight_decision(false, None, true).is_err());
        // A genuinely configured provider satisfies preview regardless of CLI.
        assert!(preflight_decision(true, None, true).is_ok());
        assert!(preflight_decision(true, Some("claude"), true).is_ok());
    }

    #[test]
    fn apply_path_preflight_still_accepts_agent_cli_fallback() {
        // Apply path: an agent CLI on PATH is an acceptable backend;
        // only the absence of BOTH a provider and a CLI is an error.
        assert!(preflight_decision(true, None, false).is_ok());
        assert!(preflight_decision(false, Some("codex"), false).is_ok());
        assert!(preflight_decision(false, None, false).is_err());
    }

    #[test]
    fn provider_preflight_uses_supported_agent_cli_order() {
        assert_eq!(
            supported_agent_cli_on_path_with(|cmd| cmd == "gemini" || cmd == "codex"),
            Some("codex")
        );
        assert_eq!(supported_agent_cli_on_path_with(|_| false), None);
    }
}
