//! Import parsed agent-file memories into the local skills store.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;

use crate::domain::models::RememberRuleInput;
use crate::infra::git::RepoScope;
use crate::skills::{
    REMEMBER_KIND_REVIEW_RULE, REMEMBER_KIND_SOFT_PREFERENCE,
    remember_as_candidate_with_confidence_for_repo, remember_for_repo,
};

use super::{AgentFileMemoryKind, registered_sources, split_memory_doc};

/// Default seed confidence for declared review rules imported from agent files.
/// They are trusted enough to show in the one-tap review queue, but deliberately
/// below active/manual confidence because imperative freeform text can be noisy.
pub const DEFAULT_AGENT_FILE_REVIEW_RULE_CONFIDENCE: f32 = 0.55;
/// Import-time "wow" budget: auto-enable only the strongest concrete project
/// rules from agent files. The rest stay in the one-tap review queue.
pub const DEFAULT_AGENT_FILE_AUTO_ACTIVE_REVIEW_RULE_LIMIT: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct AgentFileImportOptions {
    pub review_rule_confidence: f32,
    pub max_auto_active_review_rules: usize,
}

impl Default for AgentFileImportOptions {
    fn default() -> Self {
        Self {
            review_rule_confidence: DEFAULT_AGENT_FILE_REVIEW_RULE_CONFIDENCE,
            max_auto_active_review_rules: DEFAULT_AGENT_FILE_AUTO_ACTIVE_REVIEW_RULE_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentFileImportReport {
    pub docs_scanned: usize,
    pub entries_seen: usize,
    pub review_rules_active: usize,
    pub review_rules_pending: usize,
    pub soft_preferences_active: usize,
    pub reference_entries_skipped: usize,
    pub deduped: usize,
    pub sources_detected: Vec<String>,
}

pub async fn import_agent_files_for_repo(
    db: &sqlx::SqlitePool,
    repo_root: &Path,
    source_repo: &RepoScope,
) -> crate::Result<AgentFileImportReport> {
    import_agent_files_for_repo_with_options(
        db,
        repo_root,
        source_repo,
        AgentFileImportOptions::default(),
    )
    .await
}

pub async fn import_agent_files_for_repo_with_options(
    db: &sqlx::SqlitePool,
    repo_root: &Path,
    source_repo: &RepoScope,
    options: AgentFileImportOptions,
) -> crate::Result<AgentFileImportReport> {
    let mut report = AgentFileImportReport::default();
    let mut entries = Vec::new();

    for source in registered_sources() {
        if !source.detect(repo_root) {
            continue;
        }
        report.sources_detected.push(source.id().to_owned());
        let docs = source.read(repo_root)?;
        report.docs_scanned += docs.len();

        for doc in docs {
            entries.extend(split_memory_doc(&doc));
        }
    }

    report.entries_seen = entries.len();
    let auto_active_review_rules =
        auto_active_review_rule_indexes(&entries, options.max_auto_active_review_rules);

    for (idx, entry) in entries.into_iter().enumerate() {
        if entry.kind == AgentFileMemoryKind::ReviewRule && is_reference_only_entry(&entry) {
            report.reference_entries_skipped += 1;
            continue;
        }

        let kind = match entry.kind {
            AgentFileMemoryKind::ReviewRule => REMEMBER_KIND_REVIEW_RULE,
            AgentFileMemoryKind::SoftPreference => REMEMBER_KIND_SOFT_PREFERENCE,
        };
        let input = RememberRuleInput {
            title: entry.title,
            body: entry.body,
            file_patterns: (!entry.file_patterns.is_empty()).then_some(entry.file_patterns),
            bad_code: None,
            good_code: None,
            severity: None,
            kind: Some(kind.to_owned()),
            category: entry.category,
            origin: Some(format!("agent_file:{}", entry.source_id)),
            captured_by_client: Some("agent-file-import".to_owned()),
        };

        let outcome = match entry.kind {
            AgentFileMemoryKind::ReviewRule if auto_active_review_rules.contains(&idx) => {
                let outcome = remember_for_repo(db, input, source_repo).await?;
                if !outcome.deduped {
                    report.review_rules_active += 1;
                }
                outcome
            }
            AgentFileMemoryKind::ReviewRule => {
                let outcome = remember_as_candidate_with_confidence_for_repo(
                    db,
                    input,
                    options.review_rule_confidence,
                    source_repo,
                )
                .await?;
                if !outcome.deduped {
                    report.review_rules_pending += 1;
                }
                outcome
            }
            AgentFileMemoryKind::SoftPreference => {
                let outcome = remember_for_repo(db, input, source_repo).await?;
                if !outcome.deduped {
                    report.soft_preferences_active += 1;
                }
                outcome
            }
        };
        if outcome.deduped {
            report.deduped += 1;
        }
    }

    Ok(report)
}

fn auto_active_review_rule_indexes(
    entries: &[super::AgentFileMemoryEntry],
    limit: usize,
) -> HashSet<usize> {
    if limit == 0 {
        return HashSet::new();
    }

    let mut scored: Vec<(usize, i32)> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == AgentFileMemoryKind::ReviewRule)
        .filter(|(_, entry)| !is_reference_only_entry(entry))
        .filter_map(|(idx, entry)| {
            let score = auto_active_score(entry);
            (score >= 6).then_some((idx, score))
        })
        .collect();
    scored.sort_by(|(left_idx, left_score), (right_idx, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_idx.cmp(right_idx))
    });
    scored.into_iter().take(limit).map(|(idx, _)| idx).collect()
}

fn is_reference_only_entry(entry: &super::AgentFileMemoryEntry) -> bool {
    let title = entry.title.trim().to_ascii_lowercase();
    let body = entry.body.trim().to_ascii_lowercase();
    title.contains("design system")
        && body.starts_with("read `")
        && body.contains("full design system reference")
}

fn auto_active_score(entry: &super::AgentFileMemoryEntry) -> i32 {
    let title = entry.title.trim().to_ascii_lowercase();
    let body = entry.body.trim();
    let lower = body.to_ascii_lowercase();
    let mut score = 0;

    let codeish_count = body.matches('`').count()
        + body.matches("--").count()
        + body.matches("::").count()
        + body.matches('#').count()
        + body.matches("src/").count();
    score += i32::try_from(codeish_count.min(5)).unwrap_or(0);

    for needle in [
        "never ", "do not ", "only ", "must ", "always ", " no ", "use ", "prefer ",
    ] {
        if lower.contains(needle) || lower.starts_with(needle.trim_start()) {
            score += 1;
        }
    }

    if !entry.file_patterns.is_empty() {
        score += 1;
    }
    if lower.contains("token") || lower.contains("var(--") {
        score += 2;
    }
    if title.contains("border") || title.contains("radius") {
        score += 3;
    } else if title.contains("breakpoint") || title.contains("typography") {
        score += 1;
    }
    if title.contains("shadow") && lower.contains("hover") {
        score -= 2;
    }
    if body.chars().count() > 900 {
        score -= 2;
    }
    if lower.contains("exception") || lower.contains("regardless of theme") {
        score -= 2;
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tempfile::TempDir;

    async fn fresh_pool() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(SqliteConnectOptions::new().in_memory(true))
            .await
            .expect("connect sqlite");
        crate::infra::db::run_migrations(&pool)
            .await
            .expect("migrate");
        pool
    }

    #[tokio::test]
    async fn imports_only_strong_freeform_agent_file_entries_as_active_rules() {
        let db = fresh_pool().await;
        let repo = RepoScope::canonical("owner/repo").expect("repo scope");
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "- Never call `unwrap()` in `src/handlers/**/*.rs`; return typed errors instead.\n- The project uses pnpm for frontend work.",
        )
        .unwrap();

        let report = import_agent_files_for_repo(&db, tmp.path(), &repo)
            .await
            .expect("import agent files");

        assert_eq!(report.docs_scanned, 1);
        assert_eq!(report.entries_seen, 2);
        assert_eq!(report.review_rules_active, 1);
        assert_eq!(report.review_rules_pending, 1);
        assert_eq!(report.soft_preferences_active, 0);
        assert_eq!(report.reference_entries_skipped, 0);

        let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT status, type, origin, source_repo FROM skills ORDER BY status ASC, type ASC",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|(status, ty, origin, repo)| {
            status == "active"
                && ty == "review_standard"
                && origin == "agent_file:agents-md"
                && repo.as_deref() == Some("owner/repo")
        }));
        assert!(rows.iter().any(|(status, ty, origin, repo)| {
            status == "pending"
                && ty == "review_standard"
                && origin == "agent_file:agents-md"
                && repo.as_deref() == Some("owner/repo")
        }));
    }

    #[tokio::test]
    async fn skips_reference_only_agent_file_entries() {
        let db = fresh_pool().await;
        let repo = RepoScope::canonical("owner/repo").expect("repo scope");
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "# Design System\nRead `DESIGN.md` at the project root for the full design system reference. Below are the enforced conventions.",
        )
        .unwrap();

        let report = import_agent_files_for_repo(&db, tmp.path(), &repo)
            .await
            .expect("import agent files");

        assert_eq!(report.entries_seen, 1);
        assert_eq!(report.review_rules_active, 0);
        assert_eq!(report.review_rules_pending, 0);
        assert_eq!(report.reference_entries_skipped, 1);

        let row_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM skills")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(row_count, 0);
    }

    #[tokio::test]
    async fn auto_active_review_rule_limit_caps_imported_rules() {
        let db = fresh_pool().await;
        let repo = RepoScope::canonical("owner/repo").expect("repo scope");
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "\
# Tokens\nNever hardcode colors. Always use `var(--color-text)` from `src/styles/tokens.css`.\n\n\
# Radius\nOnly use `--radius-none`, `--radius-xs`, or `--radius-md`. Do not introduce 8px.\n\n\
# Breakpoints\nUse `@media (--tablet)` from `tokens.css`. Do not write raw `max-width` values.\n\n\
# Copy\nShort. Imperative. No emoji, no exclamation marks.",
        )
        .unwrap();

        let report = import_agent_files_for_repo_with_options(
            &db,
            tmp.path(),
            &repo,
            AgentFileImportOptions {
                review_rule_confidence: DEFAULT_AGENT_FILE_REVIEW_RULE_CONFIDENCE,
                max_auto_active_review_rules: 2,
            },
        )
        .await
        .expect("import agent files");

        assert_eq!(report.entries_seen, 4);
        assert_eq!(report.review_rules_active, 2);
        assert_eq!(report.review_rules_pending, 2);

        let counts: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM skills GROUP BY status ORDER BY status")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            counts,
            vec![("active".to_owned(), 2), ("pending".to_owned(), 2)]
        );
    }

    #[tokio::test]
    async fn imports_explicit_user_frontmatter_as_active_soft_preference() {
        let db = fresh_pool().await;
        let repo = RepoScope::canonical("owner/repo").expect("repo scope");
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "---\ntype: user\n---\nThe user prefers concise final answers.",
        )
        .unwrap();

        let report = import_agent_files_for_repo(&db, tmp.path(), &repo)
            .await
            .expect("import agent files");

        assert_eq!(report.docs_scanned, 1);
        assert_eq!(report.entries_seen, 1);
        assert_eq!(report.review_rules_pending, 0);
        assert_eq!(report.soft_preferences_active, 1);

        let row: (String, String, String, Option<String>) =
            sqlx::query_as("SELECT status, type, origin, source_repo FROM skills")
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            row,
            (
                "active".to_owned(),
                "soft_preference".to_owned(),
                "agent_file:agents-md".to_owned(),
                Some("owner/repo".to_owned()),
            )
        );
    }

    #[tokio::test]
    async fn review_rule_confidence_is_configurable() {
        let db = fresh_pool().await;
        let repo = RepoScope::canonical("owner/repo").expect("repo scope");
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("AGENTS.md"),
            "- Never unwrap in production request handlers.",
        )
        .unwrap();

        let report = import_agent_files_for_repo_with_options(
            &db,
            tmp.path(),
            &repo,
            AgentFileImportOptions {
                review_rule_confidence: 0.42,
                max_auto_active_review_rules: 0,
            },
        )
        .await
        .expect("import agent files");

        assert_eq!(report.review_rules_pending, 1);
        let confidence: f64 = sqlx::query_scalar(
            "SELECT confidence_score FROM skills WHERE status = 'pending' AND type = 'review_standard'",
        )
        .fetch_one(&db)
        .await
        .expect("confidence");
        assert!((confidence - 0.42).abs() < 1e-6);
    }
}
