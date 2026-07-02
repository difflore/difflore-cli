use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::time::{Duration, sleep};

use crate::runtime::CommandContext;
use crate::support::util::{exit_err, json_compact_or};

#[derive(Debug, Clone, Copy)]
pub(crate) struct BackfillSourceKindArgs {
    pub dry_run: bool,
    pub limit: Option<usize>,
    pub json: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    name: String,
    comment_url: String,
    previous_source_kind: String,
}

#[derive(Debug, Clone, Serialize)]
struct BackfillUpdate {
    id: String,
    name: String,
    comment_url: String,
    previous_source_kind: String,
    source_kind: String,
}

#[derive(Debug, Default, Serialize)]
struct BackfillSummary {
    inspected: usize,
    updates: Vec<BackfillUpdate>,
    skipped: usize,
    errors: Vec<String>,
    dry_run: bool,
}

#[derive(Debug, serde::Deserialize)]
struct GitHubCommentUser {
    login: Option<String>,
    #[serde(rename = "type")]
    type_name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct GitHubComment {
    user: Option<GitHubCommentUser>,
}

const GH_API_BACKOFF: Duration = Duration::from_millis(500);

pub(crate) async fn handle_backfill_source_kind(
    ctx: &CommandContext,
    args: BackfillSourceKindArgs,
) {
    match run(&ctx.db, args).await {
        Ok(summary) if args.json => println!("{}", json_compact_or(&summary, "{}")),
        Ok(summary) => print_summary(&summary),
        Err(err) => exit_err(&format!("memory backfill-source-kind failed: {err}")),
    }
}

async fn run(
    db: &SqlitePool,
    args: BackfillSourceKindArgs,
) -> difflore_core::Result<BackfillSummary> {
    let limit = i64::try_from(args.limit.unwrap_or(500)).unwrap_or(500);
    let rows = sqlx::query(
        "SELECT id, name, description, source_kind FROM skills \
         WHERE source_kind IN ('human', 'bot:unknown') \
           AND origin = 'pr_review' \
           AND description LIKE '%github.com/%' \
         ORDER BY installed_at ASC, id ASC \
         LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(db)
    .await?;
    let mut summary = BackfillSummary {
        dry_run: args.dry_run,
        ..BackfillSummary::default()
    };

    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        let name: String = row.try_get("name").unwrap_or_default();
        let description: String = row.try_get("description").unwrap_or_default();
        let previous_source_kind: String = row
            .try_get("source_kind")
            .unwrap_or_else(|_| "human".to_owned());
        let Some(comment_url) = extract_comment_url(&description) else {
            summary.skipped += 1;
            continue;
        };
        summary.inspected += 1;
        let candidate = Candidate {
            id,
            name,
            comment_url,
            previous_source_kind,
        };
        if summary.inspected > 1 {
            sleep(GH_API_BACKOFF).await;
        }
        match github_comment_source_kind(&candidate.comment_url).await {
            Ok(Some(source_kind)) if source_kind != candidate.previous_source_kind => {
                summary.updates.push(BackfillUpdate {
                    id: candidate.id,
                    name: candidate.name,
                    comment_url: candidate.comment_url,
                    previous_source_kind: candidate.previous_source_kind,
                    source_kind,
                });
            }
            Ok(Some(_) | None) => summary.skipped += 1,
            Err(err) => summary
                .errors
                .push(format!("{}: {}", candidate.comment_url, err)),
        }
    }

    if !args.dry_run && !summary.updates.is_empty() {
        let mut tx = db.begin().await?;
        for update in &summary.updates {
            let result = sqlx::query(
                "UPDATE skills SET source_kind = ?1 WHERE id = ?2 AND source_kind = ?3",
            )
            .bind(&update.source_kind)
            .bind(&update.id)
            .bind(&update.previous_source_kind)
            .execute(&mut *tx)
            .await?;
            if result.rows_affected() == 0 {
                continue;
            }
            sqlx::query(
                "INSERT INTO rule_events \
                 (id, skill_id, kind, source, confidence_before, confidence_after, reason, metadata) \
                 VALUES (?1, ?2, 'source_kind_backfill', 'github_api', NULL, NULL, ?3, ?4)",
            )
            .bind(backfill_event_id(&update.id))
            .bind(&update.id)
            .bind(format!(
                "source_kind {} -> {}",
                update.previous_source_kind, update.source_kind
            ))
            .bind(
                serde_json::json!({
                    "previousSourceKind": update.previous_source_kind,
                    "sourceKind": update.source_kind,
                    "commentUrl": update.comment_url,
                })
                .to_string(),
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }

    Ok(summary)
}

fn backfill_event_id(skill_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let safe_skill_id = skill_id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>();
    format!("rule-event-source-kind-backfill-{safe_skill_id}-{nanos}")
}

fn extract_comment_url(description: &str) -> Option<String> {
    description
        .lines()
        .find_map(|line| line.trim().strip_prefix("Comment:"))
        .map(str::trim)
        .filter(|url| url.contains("github.com/"))
        .map(ToOwned::to_owned)
}

fn github_api_path(comment_url: &str) -> Option<String> {
    let after_host = comment_url.split("github.com/").nth(1)?;
    let mut path_and_fragment = after_host.split('#');
    let path = path_and_fragment.next()?;
    let fragment = path_and_fragment.next()?;
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    if let Some(id) = fragment
        .strip_prefix("discussion_r")
        .filter(|id| valid_github_comment_id(id))
    {
        return Some(format!("repos/{owner}/{repo}/pulls/comments/{id}"));
    }
    if let Some(id) = fragment
        .strip_prefix("issuecomment-")
        .filter(|id| valid_github_comment_id(id))
    {
        return Some(format!("repos/{owner}/{repo}/issues/comments/{id}"));
    }
    None
}

fn valid_github_comment_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit())
}

async fn github_comment_source_kind(comment_url: &str) -> Result<Option<String>, String> {
    let Some(path) = github_api_path(comment_url) else {
        return Ok(None);
    };
    let output = Command::new("gh")
        .arg("api")
        .arg(path)
        .output()
        .await
        .map_err(|err| format!("failed to run gh api: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gh api exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let comment: GitHubComment = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("invalid gh api JSON: {err}"))?;
    let Some(user) = comment.user else {
        return Ok(None);
    };
    Ok(source_kind_from_github_user(&user))
}

fn source_kind_from_github_user(user: &GitHubCommentUser) -> Option<String> {
    let login = user
        .login
        .as_deref()
        .map(str::trim)
        .filter(|login| !login.is_empty());
    let bot_like_login = login.is_some_and(is_backfill_bot_login);
    match user.type_name.as_deref() {
        Some("Bot") => Some(format!("bot:{}", login.unwrap_or("unknown-bot"))),
        Some(_) | None if bot_like_login => Some(format!("bot:{}", login.unwrap_or("unknown-bot"))),
        Some(_) => Some("human".to_owned()),
        None => None,
    }
}

fn is_backfill_bot_login(login: &str) -> bool {
    let lower = login.to_ascii_lowercase();
    lower == "github-actions"
        || lower == "github-actions[bot]"
        || lower.ends_with("[bot]")
        || lower.ends_with("-bot")
        || lower.contains("codecov")
        || lower.contains("coderabbit")
        || lower.contains("code-rabbit")
        || lower.contains("dependabot")
        || lower.contains("renovate")
}

fn print_summary(summary: &BackfillSummary) {
    for update in &summary.updates {
        let verb = if summary.dry_run {
            "would-update"
        } else {
            "updated"
        };
        println!(
            "{verb} {}: '{}' -> {} ({})",
            update.id, update.name, update.source_kind, update.comment_url
        );
    }
    let mode = if summary.dry_run {
        "dry-run; re-run with --no-dry-run to apply"
    } else {
        "applied"
    };
    println!(
        "inspected {}, {} updates, {} skipped, {} errors ({mode})",
        summary.inspected,
        summary.updates.len(),
        summary.skipped,
        summary.errors.len()
    );
    for error in &summary.errors {
        eprintln!("error: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_api_path_parses_review_discussion_urls() {
        assert_eq!(
            github_api_path("https://github.com/owner/repo/pull/42#discussion_r123456"),
            Some("repos/owner/repo/pulls/comments/123456".to_owned())
        );
    }

    #[test]
    fn github_api_path_parses_issue_comment_urls() {
        assert_eq!(
            github_api_path("https://github.com/owner/repo/pull/42#issuecomment-7890"),
            Some("repos/owner/repo/issues/comments/7890".to_owned())
        );
    }

    #[test]
    fn github_api_path_rejects_non_comment_or_malformed_urls() {
        assert_eq!(
            github_api_path("https://github.com/owner/repo/pull/42"),
            None
        );
        assert_eq!(
            github_api_path("https://github.com/owner/repo/pull/42#discussion_r"),
            None
        );
        assert_eq!(
            github_api_path("https://example.com/owner/repo/pull/42#discussion_r123"),
            None
        );
    }

    #[test]
    fn github_user_source_kind_marks_bot_type_even_with_plain_login() {
        let user = GitHubCommentUser {
            login: Some("review-assistant".to_owned()),
            type_name: Some("Bot".to_owned()),
        };

        assert_eq!(
            source_kind_from_github_user(&user),
            Some("bot:review-assistant".to_owned())
        );
    }

    #[test]
    fn github_user_source_kind_keeps_bot_shaped_user_logins_quarantined() {
        let user = GitHubCommentUser {
            login: Some("coderabbitai[bot]".to_owned()),
            type_name: Some("User".to_owned()),
        };

        assert_eq!(
            source_kind_from_github_user(&user),
            Some("bot:coderabbitai[bot]".to_owned())
        );
    }

    #[test]
    fn github_user_source_kind_confirms_non_bot_actor_as_human() {
        let user = GitHubCommentUser {
            login: Some("alice".to_owned()),
            type_name: Some("User".to_owned()),
        };

        assert_eq!(
            source_kind_from_github_user(&user),
            Some("human".to_owned())
        );
    }
}
