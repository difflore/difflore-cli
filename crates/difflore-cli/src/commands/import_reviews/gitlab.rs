//! GitLab-specific pieces of `difflore import-reviews`: error mapping and
//! the spinner-wrapped import runner. Mirrors the GitHub sibling
//! (`github.rs` / `run_import`) so both providers read the same on the
//! terminal while the recovery hints stay provider-accurate.

use difflore_core::ingest::ImportProgress;
use difflore_core::ingest::gitlab::ImportOptions as GitlabImportOptions;
use sqlx::SqlitePool;

use crate::style;

/// Map raw GitLab REST error strings (from `ingest::gitlab::client`) into
/// actionable hints. Substring match on the `HTTP {status}` fragment the
/// client embeds; the raw string is retained on every non-trivial path so
/// triage info isn't lost.
pub(super) fn format_gitlab_import_err(label: &str, host: &str, e: &str) -> String {
    let lower = e.to_ascii_lowercase();
    if lower.contains("401") || lower.contains("unauthorized") {
        return format!(
            "{label}: GitLab rejected the token (401): invalid, expired, revoked, or missing the read_api scope.\n  \
             Mint a new PAT with read_api at {pat_url}\n  \
             then re-store it: echo \"<TOKEN>\" | difflore auth gitlab --host {host}\n  \
             Verify with `difflore auth gitlab --check --host {host}`.\n\n  raw: {e}",
            pat_url = pat_settings_url(host),
        );
    }
    if lower.contains("404") || lower.contains("not found") {
        return format!(
            "{label}: project not found or no access (404).\n  \
             GitLab returns 404 — not 403 — for private projects your token cannot see, \
             so this is either a wrong project path or a permission gap.\n  \
             Check the path (`--repo group/subgroup/project`, no .git suffix) and that the PAT \
             has the read_api scope with at least Reporter access; mint one at {pat_url}\n\n  raw: {e}",
            pat_url = pat_settings_url(host),
        );
    }
    if lower.contains("403") || lower.contains("forbidden") {
        return format!(
            "{label}: GitLab refused the request (403).\n  \
             The token authenticated but the instance blocked the call — check IP allowlists \
             or admin token policies on {host}, or mint a fresh PAT with read_api at {pat_url}\n\n  raw: {e}",
            pat_url = pat_settings_url(host),
        );
    }
    if lower.contains("429") || lower.contains("rate limit") {
        return format!(
            "{label}: GitLab rate limit hit (already retried with backoff).\n  \
             Recovery: wait a few minutes, then retry with a smaller window \
             (`--max-prs 20` or `--since YYYY-MM-DD`).\n\n  raw: {e}"
        );
    }
    if lower.contains("http 5")
        || lower.contains("bad gateway")
        || lower.contains("service unavailable")
        || lower.contains("gateway timeout")
    {
        return format!(
            "{label}: GitLab returned a server error after retrying.\n  \
             Recovery: rerun the same command, or shrink the window \
             (`--max-prs 20` or `--since YYYY-MM-DD`) if {host} is unstable.\n\n  raw: {e}"
        );
    }
    if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
        return format!(
            "{label}: TLS handshake with {host} failed.\n  \
             Self-managed instances with a private CA need that CA trusted at the OS level \
             (difflore uses the platform certificate verifier; there is no insecure-skip option).\n\n  raw: {e}"
        );
    }
    if lower.contains("timed out") || lower.contains("timeout") {
        return format!(
            "{label}: could not reach {host} in time (already retried).\n  \
             Check VPN/proxy access — self-managed instances often require the corporate \
             network — then retry; `difflore auth gitlab --check --host {host}` is a quick probe.\n\n  raw: {e}"
        );
    }
    if lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("connection refused")
        || lower.contains("connect error")
        || lower.contains("connection reset")
        || lower.contains("actively refused")
        // Windows wording for a connection reset (os error 10054).
        || lower.contains("forcibly closed")
        // reqwest's stable marker for any connect-phase failure; checked
        // after the TLS/timeout branches so it only catches the remainder.
        || lower.contains("client error (connect)")
    {
        return format!(
            "{label}: could not reach {host}.\n  \
             Check the host spelling (`--gitlab-host`), DNS, and VPN/proxy access, then retry.\n\n  raw: {e}"
        );
    }
    // Generic fallback: delegate to the product-agnostic core helper.
    difflore_core::domain::origins::format_api_error(label, e)
}

/// PAT settings page on the instance — the one URL every auth-shaped error
/// should point at.
fn pat_settings_url(host: &str) -> String {
    format!("https://{host}/-/user_settings/personal_access_tokens")
}

/// Pre-import probe (`GET /api/v4/projects/:id`): one cheap call that turns
/// auth/visibility problems into a precise error before any MR work starts.
pub(super) async fn verify_gitlab_project_access(
    host: &str,
    token: &str,
    project_path: &str,
) -> Result<(), String> {
    difflore_core::ingest::gitlab::verify_project_access(host, token, project_path)
        .await
        .map_err(|e| format_gitlab_import_err("Import failed", host, &e.to_string()))
}

/// Spinner-wrapped GitLab import (mirror of the GitHub `run_import`).
pub(super) async fn run_gitlab_import(
    db: &SqlitePool,
    opts: GitlabImportOptions,
    upload: bool,
    json: bool,
) -> Result<ImportProgress, String> {
    let host = opts.host.clone();
    let source_label = format!("{}/{}", opts.host, opts.project_path);

    if json {
        return match difflore_core::ingest::gitlab::import_mr_reviews(db, opts, None).await {
            Ok(result) => Ok(result),
            Err(e) => Err(format_gitlab_import_err(
                "Import failed",
                &host,
                &e.to_string(),
            )),
        };
    }

    let spinner = style::Spinner::new(&format!("Importing MR reviews from {source_label}"));
    let spinner_progress = spinner.handle();

    let direct_mr_mode = !opts.mr_iids.is_empty();
    let progress_cb: Box<dyn Fn(&ImportProgress) + Send> = Box::new(move |p| {
        if p.prs_total > 0 && p.prs_fetched > 0 {
            let skipped_part = if p.comments_skipped > 0 {
                format!(" ({} skipped)", p.comments_skipped)
            } else {
                String::new()
            };
            spinner_progress.println(&format!(
                "  [{}/{}] {} comments imported{}",
                p.prs_fetched, p.prs_total, p.comments_imported, skipped_part
            ));
        } else if p.prs_total > 0 {
            spinner_progress.println(&format!(
                "  {} MRs with review discussions to import",
                p.prs_total
            ));
        } else if direct_mr_mode && p.prs_missing > 0 {
            spinner_progress.println(&format!(
                "  No requested MRs with review discussions found ({} missing/inaccessible).",
                p.prs_missing
            ));
        } else {
            spinner_progress.println("  No merged MRs with review discussions found.");
        }
    });

    let result =
        match difflore_core::ingest::gitlab::import_mr_reviews(db, opts, Some(progress_cb)).await {
            Ok(result) => result,
            Err(e) => {
                spinner.finish_err("Import failed");
                return Err(format_gitlab_import_err(
                    "Import failed",
                    &host,
                    &e.to_string(),
                ));
            }
        };

    spinner.finish_ok(&format!(
        "Imported {} MRs from {}",
        result.prs_fetched, source_label,
    ));
    println!("  review comments:        {}", result.comments_imported);
    if result.comments_skipped > 0 {
        println!("  skipped:                {}", result.comments_skipped);
    }
    if result.prs_missing > 0 {
        let missing = result
            .missing_pr_numbers
            .iter()
            .map(|n| format!("!{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  missing MRs:            {missing}");
    }
    // Phrase as "requested": upload runs after this summary, so a later
    // failure must not contradict an earlier "uploaded: yes".
    println!(
        "  upload requested:       {}",
        if upload { "yes" } else { "no" }
    );
    println!();
    if upload {
        println!(
            "  {} Uploading imported comments for extraction...",
            style::emerald(style::sym::TIP),
        );
    } else if result.comments_imported > 0 {
        println!(
            "  {} Imports stayed local.",
            style::emerald(style::sym::TIP),
        );
        style::println_wrapped("    Drafting review candidates from high-signal comments...");
    }
    Ok(result)
}
