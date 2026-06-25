//! Which rules participate in a static export.
//!
//! The membership rule mirrors runtime recall (Project Scope Invariant: a
//! rule is served only to the repo it was learned from), with one deliberate
//! addition: explicit local rules (`source = 'local'` with no `source_repo`)
//! are included because the user typed them for this machine, not for some
//! other repo. The scope predicate itself ([`repo_scope_matches`]) is the
//! shared core version of `recall`'s exact-match check so the two surfaces
//! cannot drift.

use sqlx::SqlitePool;

use crate::context::rule_source::{
    RuleExample, load_rule_examples_batch, repo_scope_from_source_repo,
};
use crate::error::CoreError;

/// `skills.source` values that exist only because a cloud/team sync wrote
/// them. `--local-only` removes these so an export cannot become a
/// "download the team corpus then unsubscribe" artifact by accident.
const SYNCED_SOURCES: &[&str] = &["cloud", "team"];

/// One rule fully resolved for static export rendering.
#[derive(Debug, Clone)]
pub struct ExportRule {
    pub id: String,
    pub name: String,
    pub description: String,
    pub r#type: String,
    pub confidence: f64,
    pub origin: String,
    pub source: String,
    /// Canonical lower-cased `owner/repo` the rule was learned from, when the
    /// stored `source_repo` parses. `None` = explicit local rule.
    pub repo_scope: Option<String>,
    pub check_prompt: Option<String>,
    pub file_patterns: Vec<String>,
    /// Bad/Good example pairs (empty when none exist or examples are skipped).
    pub examples: Vec<RuleExample>,
}

/// Collection knobs, one per CLI flag.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportCollectOptions<'a> {
    /// Per-format engine gate: `claude-md` exports only rules with
    /// `enabled_for_claude = 1` (parity with the hook/MCP claude engine
    /// filter); `agents-md` passes `None` and takes every active rule.
    pub engine: Option<&'a str>,
    /// Exclude team/cloud-synced rules (`--local-only`).
    pub local_only: bool,
    /// Load Bad/Good examples (`--no-examples` turns this off).
    pub include_examples: bool,
    /// Cap the export to the first N in-scope rules of the deterministic
    /// order (`--max-rules`); `None` = unlimited. Applied before example
    /// loading so capped-away rules cost no extra queries.
    pub max_rules: Option<usize>,
}

/// Result of a collection pass: the in-scope rules in a deterministic order
/// (name, then id) plus the repo scopes the filter ran against.
#[derive(Debug, Clone)]
pub struct ExportCollection {
    pub rules: Vec<ExportRule>,
    pub repo_scopes: Vec<String>,
    /// In-scope rule count before any `max_rules` cap, so report surfaces can
    /// say "exported N of M" when truncation happened
    /// (`total_in_scope > rules.len()`).
    pub total_in_scope: usize,
}

/// Project-scope predicate shared by `recall` exact-title matching and static
/// export: a rule participates only when its canonical repo scope
/// exact-matches (case-insensitively) one of the scopes detected from the
/// current project's git remotes. A rule without a repo scope never matches —
/// scope-less metadata is not a global wildcard, and an empty scope list
/// matches nothing (Project Scope Invariant: no repo identity, no recall).
#[must_use]
pub fn repo_scope_matches(rule_repo_scope: Option<&str>, repo_scopes: &[String]) -> bool {
    if repo_scopes.is_empty() {
        return false;
    }
    let Some(scope) = rule_repo_scope else {
        return false;
    };
    repo_scopes
        .iter()
        .any(|candidate| scope.eq_ignore_ascii_case(candidate))
}

/// An explicit local rule: typed/captured on this machine (`source = 'local'`)
/// and never attributed to a repo (`source_repo` NULL/blank). These export
/// alongside repo-scoped rules because the user authored them directly.
#[must_use]
pub fn is_explicit_local_rule(source: &str, source_repo: Option<&str>) -> bool {
    source == "local" && source_repo.map(str::trim).is_none_or(str::is_empty)
}

#[derive(sqlx::FromRow)]
struct ExportRuleRow {
    id: String,
    name: String,
    description: String,
    r#type: String,
    confidence_score: f64,
    origin: String,
    source: String,
    source_repo: Option<String>,
    check_prompt: Option<String>,
    file_patterns: Option<String>,
}

/// Collect the export rule set for the project at `project_root`. Repo scopes
/// come from the same remote detection the recall orchestrator uses
/// (`infra::git::detect_repo_full_names_with_gitlab_hosts`: `origin` first, then
/// `upstream`).
pub async fn collect_rules_for_export(
    db: &SqlitePool,
    project_root: &std::path::Path,
    opts: ExportCollectOptions<'_>,
) -> Result<ExportCollection, CoreError> {
    let configured_gitlab_hosts = crate::ingest::gitlab::auth::configured_hosts().await;
    let repo_scopes = crate::infra::git::detect_repo_full_names_with_gitlab_hosts(
        &project_root.to_string_lossy(),
        &configured_gitlab_hosts,
    );
    collect_rules_for_export_with_scopes(db, &repo_scopes, opts).await
}

/// Scope-injected variant of [`collect_rules_for_export`] for callers (and
/// tests) that already resolved the repo scopes.
pub async fn collect_rules_for_export_with_scopes(
    db: &SqlitePool,
    repo_scopes: &[String],
    opts: ExportCollectOptions<'_>,
) -> Result<ExportCollection, CoreError> {
    // Fixed clause per engine — never interpolate the raw engine string.
    // Mirrors `rule_source::load_rules_from_db_for_engine` (status gate +
    // per-engine enable flags); unknown engines fall back to no engine filter.
    let engine_clause = match opts.engine {
        Some("codex") => " AND enabled_for_codex = 1",
        Some("claude") => " AND enabled_for_claude = 1",
        Some("gemini") => " AND enabled_for_gemini = 1",
        Some("cursor") => " AND enabled_for_cursor = 1",
        _ => "",
    };
    // Pending candidates MUST NOT export: they exist for review, not for
    // injection into agent context (same rule as the recall loader).
    // Deterministic ORDER BY keeps re-exports byte-stable so the content-hash
    // short-circuit in writeback actually fires.
    let sql = format!(
        "SELECT id, name, description, type as \"type\", confidence_score, origin, source, \
         source_repo, check_prompt, file_patterns \
         FROM skills WHERE status = 'active'{engine_clause} \
         ORDER BY name COLLATE NOCASE, id"
    );
    let rows = sqlx::query_as::<_, ExportRuleRow>(&sql)
        .fetch_all(db)
        .await?;

    let mut rules: Vec<ExportRule> = rows
        .into_iter()
        .filter_map(|row| {
            let repo_scope = repo_scope_from_source_repo(row.source_repo.as_deref());
            let in_scope = repo_scope_matches(repo_scope.as_deref(), repo_scopes)
                || is_explicit_local_rule(&row.source, row.source_repo.as_deref());
            if !in_scope {
                return None;
            }
            if opts.local_only && SYNCED_SOURCES.contains(&row.source.as_str()) {
                return None;
            }
            let file_patterns: Vec<String> = row
                .file_patterns
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            Some(ExportRule {
                id: row.id,
                name: row.name,
                description: row.description,
                r#type: row.r#type,
                confidence: row.confidence_score,
                origin: row.origin,
                source: row.source,
                repo_scope,
                check_prompt: row.check_prompt,
                file_patterns,
                examples: Vec::new(),
            })
        })
        .collect();

    // `--max-rules` cap: keep the first N of the deterministic ORDER BY
    // above, so the capped set is stable across re-exports (the content-hash
    // short-circuit still fires) and is always a prefix of the full export.
    let total_in_scope = rules.len();
    if let Some(cap) = opts.max_rules {
        rules.truncate(cap);
    }

    if opts.include_examples && !rules.is_empty() {
        let ids: Vec<String> = rules.iter().map(|rule| rule.id.clone()).collect();
        let mut examples_map = load_rule_examples_batch(db, &ids).await?;
        for rule in &mut rules {
            if let Some(examples) = examples_map.remove(&rule.id) {
                rule.examples = examples;
            }
        }
    }

    Ok(ExportCollection {
        rules,
        repo_scopes: repo_scopes.to_vec(),
        total_in_scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn repo_scope_matches_requires_scope_on_both_sides() {
        let scopes = vec!["acme/widgets".to_owned()];
        assert!(repo_scope_matches(Some("acme/widgets"), &scopes));
        // Case-insensitive exact match, same as recall.
        assert!(repo_scope_matches(Some("Acme/Widgets"), &scopes));
        // A scope-less rule is not a global wildcard.
        assert!(!repo_scope_matches(None, &scopes));
        // No detected repo identity -> nothing matches.
        assert!(!repo_scope_matches(Some("acme/widgets"), &[]));
        // Different repo -> no cross-repo recall.
        assert!(!repo_scope_matches(Some("vitejs/vite"), &scopes));
    }

    #[test]
    fn explicit_local_rule_requires_local_source_and_no_repo() {
        assert!(is_explicit_local_rule("local", None));
        assert!(is_explicit_local_rule("local", Some("  ")));
        assert!(!is_explicit_local_rule("local", Some("acme/widgets")));
        assert!(!is_explicit_local_rule("cloud", None));
        assert!(!is_explicit_local_rule("github", None));
    }

    async fn pool() -> SqlitePool {
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite::memory:")
            .expect("memory sqlite opts");
        let pool = SqlitePool::connect_with(opts).await.expect("connect");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    #[allow(clippy::too_many_arguments)] // reason: test fixture mirrors the skills columns under test.
    async fn insert_rule(
        pool: &SqlitePool,
        id: &str,
        name: &str,
        source: &str,
        source_repo: Option<&str>,
        status: &str,
        enabled_for_claude: i64,
        check_prompt: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO skills (id, name, source, directory, version, description, type, \
             source_repo, status, enabled_for_claude, check_prompt) \
             VALUES (?1, ?2, ?3, ?4, '1.0.0', ?5, 'review_standard', ?6, ?7, ?8, ?9)",
        )
        .bind(id)
        .bind(name)
        .bind(source)
        .bind(id)
        .bind(format!("Body of {name}"))
        .bind(source_repo)
        .bind(status)
        .bind(enabled_for_claude)
        .bind(check_prompt)
        .execute(pool)
        .await
        .expect("insert skill");
    }

    fn scopes() -> Vec<String> {
        vec!["acme/widgets".to_owned()]
    }

    #[tokio::test]
    async fn collect_keeps_in_scope_and_explicit_local_rules_only() {
        let pool = pool().await;
        insert_rule(
            &pool,
            "r-scoped",
            "Scoped",
            "cloud",
            Some("acme/widgets"),
            "active",
            1,
            None,
        )
        .await;
        insert_rule(
            &pool,
            "r-other",
            "Other repo",
            "cloud",
            Some("vitejs/vite"),
            "active",
            1,
            None,
        )
        .await;
        insert_rule(&pool, "r-local", "Local", "local", None, "active", 1, None).await;
        insert_rule(
            &pool,
            "r-pending",
            "Pending",
            "cloud",
            Some("acme/widgets"),
            "pending",
            1,
            None,
        )
        .await;

        let got =
            collect_rules_for_export_with_scopes(&pool, &scopes(), ExportCollectOptions::default())
                .await
                .expect("collect");

        let ids: Vec<&str> = got.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["r-local", "r-scoped"]);
        assert_eq!(
            got.rules[1].repo_scope.as_deref(),
            Some("acme/widgets"),
            "scoped rule keeps canonical repo scope for the learned-from line"
        );
    }

    #[tokio::test]
    async fn collect_local_only_excludes_synced_sources() {
        let pool = pool().await;
        insert_rule(
            &pool,
            "r-cloud",
            "Cloud",
            "cloud",
            Some("acme/widgets"),
            "active",
            1,
            None,
        )
        .await;
        insert_rule(
            &pool,
            "r-team",
            "Team",
            "team",
            Some("acme/widgets"),
            "active",
            1,
            None,
        )
        .await;
        insert_rule(&pool, "r-local", "Local", "local", None, "active", 1, None).await;

        let got = collect_rules_for_export_with_scopes(
            &pool,
            &scopes(),
            ExportCollectOptions {
                local_only: true,
                ..Default::default()
            },
        )
        .await
        .expect("collect");

        let ids: Vec<&str> = got.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["r-local"]);
    }

    #[tokio::test]
    async fn collect_engine_gate_filters_claude_disabled_rules() {
        let pool = pool().await;
        insert_rule(
            &pool,
            "r-on",
            "Claude on",
            "cloud",
            Some("acme/widgets"),
            "active",
            1,
            None,
        )
        .await;
        insert_rule(
            &pool,
            "r-off",
            "Claude off",
            "cloud",
            Some("acme/widgets"),
            "active",
            0,
            None,
        )
        .await;

        let claude = collect_rules_for_export_with_scopes(
            &pool,
            &scopes(),
            ExportCollectOptions {
                engine: Some("claude"),
                ..Default::default()
            },
        )
        .await
        .expect("collect claude");
        let ids: Vec<&str> = claude.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["r-on"]);

        // agents-md takes everything active regardless of engine flags.
        let agents =
            collect_rules_for_export_with_scopes(&pool, &scopes(), ExportCollectOptions::default())
                .await
                .expect("collect agents");
        assert_eq!(agents.rules.len(), 2);
    }

    #[tokio::test]
    async fn collect_empty_scopes_yields_only_explicit_local() {
        let pool = pool().await;
        insert_rule(
            &pool,
            "r-scoped",
            "Scoped",
            "cloud",
            Some("acme/widgets"),
            "active",
            1,
            None,
        )
        .await;
        insert_rule(&pool, "r-local", "Local", "local", None, "active", 1, None).await;

        let got = collect_rules_for_export_with_scopes(&pool, &[], ExportCollectOptions::default())
            .await
            .expect("collect");
        let ids: Vec<&str> = got.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["r-local"],
            "no repo identity must not widen to the whole machine corpus"
        );
    }

    #[tokio::test]
    async fn collect_max_rules_caps_to_deterministic_prefix() {
        let pool = pool().await;
        // Names chosen so the deterministic order (name NOCASE, then id) is
        // Alpha < bravo < Charlie regardless of case.
        insert_rule(&pool, "r-c", "Charlie", "local", None, "active", 1, None).await;
        insert_rule(&pool, "r-a", "Alpha", "local", None, "active", 1, None).await;
        insert_rule(&pool, "r-b", "bravo", "local", None, "active", 1, None).await;

        let capped = collect_rules_for_export_with_scopes(
            &pool,
            &[],
            ExportCollectOptions {
                max_rules: Some(2),
                ..Default::default()
            },
        )
        .await
        .expect("collect capped");
        let ids: Vec<&str> = capped.rules.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["r-a", "r-b"], "cap keeps the ordered prefix");
        assert_eq!(
            capped.total_in_scope, 3,
            "pre-cap total must be reported for the truncation flag"
        );

        // Default (None) stays unlimited.
        let unlimited =
            collect_rules_for_export_with_scopes(&pool, &[], ExportCollectOptions::default())
                .await
                .expect("collect unlimited");
        assert_eq!(unlimited.rules.len(), 3);
        assert_eq!(unlimited.total_in_scope, 3);

        // A cap >= the in-scope count changes nothing.
        let roomy = collect_rules_for_export_with_scopes(
            &pool,
            &[],
            ExportCollectOptions {
                max_rules: Some(10),
                ..Default::default()
            },
        )
        .await
        .expect("collect roomy");
        assert_eq!(roomy.rules.len(), 3);
        assert_eq!(roomy.total_in_scope, 3);
    }

    #[tokio::test]
    async fn collect_max_rules_skips_example_loading_for_capped_rules() {
        let pool = pool().await;
        insert_rule(&pool, "r-a", "Alpha", "local", None, "active", 1, None).await;
        insert_rule(&pool, "r-b", "Bravo", "local", None, "active", 1, None).await;
        for (ex, skill) in [("ex-a", "r-a"), ("ex-b", "r-b")] {
            sqlx::query(
                "INSERT INTO rule_examples (id, skill_id, bad_code, good_code, description) \
                 VALUES (?1, ?2, 'bad()', 'good()', 'why')",
            )
            .bind(ex)
            .bind(skill)
            .execute(&pool)
            .await
            .expect("insert example");
        }

        let got = collect_rules_for_export_with_scopes(
            &pool,
            &[],
            ExportCollectOptions {
                include_examples: true,
                max_rules: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("collect");
        assert_eq!(got.rules.len(), 1);
        assert_eq!(got.rules[0].id, "r-a");
        assert_eq!(
            got.rules[0].examples.len(),
            1,
            "kept rule still gets its examples"
        );
    }

    #[tokio::test]
    async fn collect_examples_load_only_when_requested() {
        let pool = pool().await;
        insert_rule(
            &pool,
            "r-ex",
            "With example",
            "local",
            None,
            "active",
            1,
            Some("Did you check?"),
        )
        .await;
        sqlx::query(
            "INSERT INTO rule_examples (id, skill_id, bad_code, good_code, description) \
             VALUES ('ex1', 'r-ex', 'bad()', 'good()', 'why')",
        )
        .execute(&pool)
        .await
        .expect("insert example");

        let with = collect_rules_for_export_with_scopes(
            &pool,
            &[],
            ExportCollectOptions {
                include_examples: true,
                ..Default::default()
            },
        )
        .await
        .expect("collect with examples");
        assert_eq!(with.rules[0].examples.len(), 1);
        assert_eq!(
            with.rules[0].check_prompt.as_deref(),
            Some("Did you check?")
        );

        let without =
            collect_rules_for_export_with_scopes(&pool, &[], ExportCollectOptions::default())
                .await
                .expect("collect without examples");
        assert!(without.rules[0].examples.is_empty());
    }
}
