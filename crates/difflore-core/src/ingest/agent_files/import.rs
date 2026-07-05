//! Import parsed agent-file memories into the local skills store.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;

use crate::domain::models::RememberRuleInput;
use crate::infra::git::RepoScope;
use crate::skills::{
    REMEMBER_KIND_REVIEW_RULE, REMEMBER_KIND_SOFT_PREFERENCE,
    remember_as_candidate_with_confidence_for_repo,
    remember_as_candidate_with_confidence_for_repo_and_source_kind, remember_for_repo,
};

use super::{
    AgentFileMemoryKind, CLAUDE_CODE_MEMORY_SOURCE_ID, registered_sources, split_memory_doc,
};

/// Default seed confidence for declared review rules imported from agent files.
/// They are trusted enough to show in the one-tap review queue, but deliberately
/// below active/manual confidence because imperative freeform text can be noisy.
pub const DEFAULT_AGENT_FILE_REVIEW_RULE_CONFIDENCE: f32 = 0.55;
/// Import-time "wow" budget: auto-enable only the strongest concrete project
/// rules from agent files. The rest stay in the one-tap review queue.
pub const DEFAULT_AGENT_FILE_AUTO_ACTIVE_REVIEW_RULE_LIMIT: usize = 3;
const CLAUDE_CODE_AUTO_MEMORY_ORIGIN: &str = "agent-memory";
const CLAUDE_CODE_AUTO_MEMORY_SOURCE_KIND: &str = "bot:claude-code-memory";

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
    pub claude_code_auto_memory_candidates: usize,
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
        let is_claude_code_auto_memory = entry.source_id == CLAUDE_CODE_MEMORY_SOURCE_ID;
        let entry_kind = if is_claude_code_auto_memory {
            AgentFileMemoryKind::ReviewRule
        } else {
            entry.kind
        };

        if entry_kind == AgentFileMemoryKind::ReviewRule && is_reference_only_entry(&entry) {
            report.reference_entries_skipped += 1;
            continue;
        }

        let kind = match entry_kind {
            AgentFileMemoryKind::ReviewRule => REMEMBER_KIND_REVIEW_RULE,
            AgentFileMemoryKind::SoftPreference => REMEMBER_KIND_SOFT_PREFERENCE,
        };
        let origin = if is_claude_code_auto_memory {
            CLAUDE_CODE_AUTO_MEMORY_ORIGIN.to_owned()
        } else {
            format!("agent_file:{}", entry.source_id)
        };
        let captured_by_client = if is_claude_code_auto_memory {
            "claude-code-auto-memory-import"
        } else {
            "agent-file-import"
        };
        let body = if is_claude_code_auto_memory {
            body_with_source_file_provenance(&entry.body, &entry.path)
        } else {
            entry.body
        };
        let input = RememberRuleInput {
            title: entry.title,
            body,
            file_patterns: (!entry.file_patterns.is_empty()).then_some(entry.file_patterns),
            bad_code: None,
            good_code: None,
            severity: None,
            kind: Some(kind.to_owned()),
            category: entry.category,
            origin: Some(origin),
            captured_by_client: Some(captured_by_client.to_owned()),
        };

        let outcome = match entry_kind {
            AgentFileMemoryKind::ReviewRule if is_claude_code_auto_memory => {
                let outcome = remember_as_candidate_with_confidence_for_repo_and_source_kind(
                    db,
                    input,
                    options.review_rule_confidence,
                    source_repo,
                    Some(CLAUDE_CODE_AUTO_MEMORY_SOURCE_KIND),
                )
                .await?;
                if !outcome.deduped {
                    report.review_rules_pending += 1;
                    report.claude_code_auto_memory_candidates += 1;
                }
                outcome
            }
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

fn body_with_source_file_provenance(body: &str, path: &Path) -> String {
    format!(
        "Rule:\n{}\n\nSource evidence:\nSource: Claude Code auto-memory\nFile: {}",
        body.trim(),
        path.display()
    )
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
        .filter(|(_, entry)| entry.source_id != CLAUDE_CODE_MEMORY_SOURCE_ID)
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
    use crate::skills::list_candidates;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::path::Path;
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

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    fn claude_project_slug(repo_root: &Path) -> String {
        let canonical = repo_root.canonicalize().expect("canonical repo path");
        canonical
            .to_string_lossy()
            .chars()
            .map(|ch| match ch {
                '\\' | '/' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '-',
                _ => ch,
            })
            .collect()
    }

    fn claude_memory_dir(home: &Path, repo_root: &Path) -> std::path::PathBuf {
        home.join(".claude")
            .join("projects")
            .join(claude_project_slug(repo_root))
            .join("memory")
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

    #[test]
    fn imports_claude_code_auto_memory_as_pending_candidates_with_source_file_provenance() {
        let rt = runtime();
        let home = TempDir::new().unwrap();
        let repo_root = TempDir::new().unwrap();
        let memory = claude_memory_dir(home.path(), repo_root.path());
        std::fs::create_dir_all(&memory).unwrap();
        let source_file = memory.join("api-rules.md");
        std::fs::write(
            &source_file,
            "# API handlers\nNever call `unwrap()` in `src/api/**/*.rs`; return typed errors instead.",
        )
        .unwrap();
        std::fs::write(memory.join("MEMORY.md"), "- [API handlers](api-rules.md)").unwrap();

        temp_env::with_var("HOME", Some(home.path().as_os_str()), || {
            temp_env::with_var("DIFFLORE_CLAUDE_HOME", None::<&str>, || {
                rt.block_on(async {
                    let db = fresh_pool().await;
                    let repo = RepoScope::canonical("owner/repo").expect("repo scope");

                    let report = import_agent_files_for_repo(&db, repo_root.path(), &repo)
                        .await
                        .expect("import agent files");

                    assert_eq!(report.docs_scanned, 2);
                    assert_eq!(report.entries_seen, 1);
                    assert_eq!(report.review_rules_active, 0);
                    assert_eq!(report.review_rules_pending, 1);
                    assert_eq!(report.claude_code_auto_memory_candidates, 1);
                    assert_eq!(report.reference_entries_skipped, 0);
                    assert!(
                        report
                            .sources_detected
                            .contains(&CLAUDE_CODE_MEMORY_SOURCE_ID.to_owned())
                    );

                    let row: (String, String, String, String, String, Option<String>) =
                        sqlx::query_as(
                            "SELECT status, type, origin, source_kind, description, source_repo \
                             FROM skills",
                        )
                        .fetch_one(&db)
                        .await
                        .unwrap();
                    assert_eq!(row.0, "pending");
                    assert_eq!(row.1, "review_standard");
                    assert_eq!(row.2, CLAUDE_CODE_AUTO_MEMORY_ORIGIN);
                    assert_eq!(row.3, CLAUDE_CODE_AUTO_MEMORY_SOURCE_KIND);
                    assert!(row.4.contains("Source: Claude Code auto-memory"));
                    assert!(row.4.contains(&source_file.display().to_string()));
                    assert_eq!(row.5.as_deref(), Some("owner/repo"));

                    let candidates = list_candidates(&db, Some("owner/repo"), None)
                        .await
                        .unwrap();
                    assert_eq!(candidates.len(), 1);
                    let source_path = source_file.display().to_string();
                    assert_eq!(
                        candidates[0]
                            .source_proof
                            .as_ref()
                            .and_then(|proof| proof.file.as_deref()),
                        Some(source_path.as_str())
                    );

                    let second = import_agent_files_for_repo(&db, repo_root.path(), &repo)
                        .await
                        .expect("second import dedupes");
                    assert_eq!(second.claude_code_auto_memory_candidates, 0);
                    assert_eq!(second.deduped, 1);
                    assert_eq!(
                        crate::skills::count_pending_candidates(&db, Some("owner/repo"))
                            .await
                            .unwrap(),
                        1
                    );
                });
            });
        });
    }

    #[test]
    fn missing_claude_code_auto_memory_dir_reports_zero() {
        let rt = runtime();
        let home = TempDir::new().unwrap();
        let repo_root = TempDir::new().unwrap();

        temp_env::with_var("HOME", Some(home.path().as_os_str()), || {
            temp_env::with_var("DIFFLORE_CLAUDE_HOME", None::<&str>, || {
                rt.block_on(async {
                    let db = fresh_pool().await;
                    let repo = RepoScope::canonical("owner/repo").expect("repo scope");

                    let report = import_agent_files_for_repo(&db, repo_root.path(), &repo)
                        .await
                        .expect("import agent files");

                    assert_eq!(report.docs_scanned, 0);
                    assert_eq!(report.entries_seen, 0);
                    assert_eq!(report.claude_code_auto_memory_candidates, 0);
                    assert!(
                        !report
                            .sources_detected
                            .contains(&CLAUDE_CODE_MEMORY_SOURCE_ID.to_owned())
                    );
                });
            });
        });
    }
}
