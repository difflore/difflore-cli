use sqlx::SqlitePool;

use crate::error::CoreError;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RuleDocument {
    pub skill_id: String,
    pub title: String,
    pub content: String,
    pub confidence: f64,
    /// JSON-serialised glob list (e.g. `["**/*.rs", "tokio/src/io/**"]`).
    /// Empty / NULL = universal rule (cascade treats as always-matching).
    pub file_patterns: Option<String>,
    /// Derived from `tags` JSON. NULL means no language hint; SQL filters keep
    /// NULL rows eligible unless an exact language match is required.
    pub language: Option<String>,
    /// Derived from canonical `source_repo`. NULL is unattributed metadata, not
    /// a runtime global rule; recall must exact-match the current repo/project.
    pub repo_scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftPreferenceDocument {
    pub skill_id: String,
    pub title: String,
    pub body: String,
    pub source_repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleIndexState {
    pub rule_count: i64,
    pub max_updated_at: Option<String>,
    pub embedding_profile: String,
    /// Stable identity of the in-scope rule SET served for the current git
    /// repo scope. `rule_count` + `max_updated_at` alone cannot detect a scope
    /// swap (same count and timestamp, different membership), so without this
    /// the freshness check could skip a re-index and serve the wrong scope's
    /// chunks. `None` means scope-agnostic (whole active corpus) and is ignored
    /// by the freshness comparison.
    pub scope_signature: Option<String>,
}

/// Derive a stable signature for an in-scope rule SET from its skill ids.
/// Order-independent (ids sorted before hashing) so the signature depends only
/// on membership. Returns `None` for an empty set so the freshness check stays
/// scope-agnostic.
pub fn scope_signature_from_skill_ids<'a>(
    skill_ids: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    use sha1::{Digest, Sha1};
    let mut ids: Vec<&str> = skill_ids.into_iter().collect();
    if ids.is_empty() {
        return None;
    }
    ids.sort_unstable();
    ids.dedup();
    let mut hasher = Sha1::new();
    for id in ids {
        hasher.update(id.as_bytes());
        // Length-delimit so ["ab", "c"] and ["a", "bc"] cannot collide.
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Some(hex)
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: String,
    name: String,
    description: String,
    r#type: String,
    tags: String,
    confidence_score: f64,
    file_patterns: Option<String>,
    source_repo: Option<String>,
}

/// Language tags recognised inside a skill's `tags` JSON, matched
/// case-insensitively. First hit wins, so order controls priority — most
/// common languages first.
const LANGUAGE_TAGS: &[&str] = &[
    "rust",
    "typescript",
    "javascript",
    "python",
    "go",
    "java",
    "kotlin",
    "swift",
    "ruby",
    "php",
    "cpp",
    "c++",
    "csharp",
    "c#",
    "c",
];

/// Extract a language from a skill's `tags` JSON (a stringified array like
/// `["rust", "async"]`). Returns the first recognised language tag (lower-cased)
/// or `None` if the tags are unparseable, empty, or carry no language hint.
///
/// Unknown tags are ignored rather than guessed at: a false `language` hint
/// would silently drop real hits at retrieval time.
pub fn language_from_tags(tags_json: &str) -> Option<String> {
    let trimmed = tags_json.trim();
    if trimmed.is_empty() {
        return None;
    }
    let tags: Vec<String> = serde_json::from_str(trimmed).ok()?;
    for tag in tags {
        let lower = tag.trim().to_ascii_lowercase();
        if LANGUAGE_TAGS.iter().any(|known| *known == lower) {
            // Normalise to a single canonical spelling for downstream filters.
            let canonical = match lower.as_str() {
                "c++" => "cpp".to_owned(),
                "c#" => "csharp".to_owned(),
                other => other.to_owned(),
            };
            return Some(canonical);
        }
    }
    None
}

/// Derive a confidence multiplier from a rule's tags. Two evidence signals:
///   - `cluster-size:N` — distinct review extractions clustered into this rule
///     (N=1 is weakest; N>=3 is corroborated).
///   - `severity:{error,warning,info}` — severity attached at extraction.
///
/// Returns a value in `[0.4, 0.95]` to multiply against the retrieval score,
/// or `None` when neither tag is present (caller keeps its 0.7 default).
pub fn confidence_from_tags(tags_json: &str) -> Option<f64> {
    let trimmed = tags_json.trim();
    if trimmed.is_empty() {
        return None;
    }
    let tags: Vec<String> = serde_json::from_str(trimmed).ok()?;
    let mut cluster_size: Option<u32> = None;
    let mut malformed_cluster_size = false;
    let mut severity: Option<String> = None;
    for tag in &tags {
        let lower = tag.trim().to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("cluster-size:") {
            if let Ok(n) = rest.parse::<u32>() {
                cluster_size = Some(n);
            } else {
                malformed_cluster_size = true;
            }
        } else if let Some(rest) = lower.strip_prefix("severity:") {
            severity = Some(rest.to_owned());
        }
    }
    if cluster_size.is_none() && severity.is_none() && !malformed_cluster_size {
        return None;
    }
    let base_score = if let Some(n) = cluster_size {
        match n {
            0 | 1 => 0.55, // singleton — downweight, but not under the 0.2 floor
            2 => 0.7,
            3..=4 => 0.8,
            _ => 0.9, // 5+ corroborating extractions
        }
    } else if malformed_cluster_size {
        0.4
    } else {
        0.7
    };
    let score = if let Some(sev) = severity.as_deref() {
        match sev {
            "error" => f64::min(base_score + 0.05, 0.95),
            "info" => f64::max(base_score - 0.05, 0.4),
            _ => base_score, // warning is the neutral default
        }
    } else {
        base_score
    };
    Some(score)
}

/// Canonical lowercase file-extension → language tag table shared by the
/// path-based detector (`retrieval::detect_language_from_path`) and the
/// glob-pattern detector (`language_from_pattern`). The spelling matches the
/// language tags used elsewhere so filters round-trip. Unknown extensions
/// return `None`.
///
/// Note the C-family entries (`c`/`h`/`hh`) are intentionally only honoured by
/// the path detector; `language_from_pattern` excludes them (see there) so its
/// observable behaviour is unchanged.
pub(crate) fn extension_to_language(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "rb" => "ruby",
        "php" => "php",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => "cpp",
        "c" | "h" => "c",
        "cs" => "csharp",
        _ => return None,
    })
}

/// Map a single glob pattern to a canonical language tag if it uniquely
/// identifies one. Patterns spanning multiple languages (`"**/*"`,
/// `"**/test*"`) return None.
fn language_from_pattern(p: &str) -> Option<&'static str> {
    let lower = p.to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    if ext == lower || ext.contains('/') || ext.contains('*') {
        return None;
    }
    // C-family extensions are ambiguous between C/C++ for a bare glob (`*.h`),
    // so this glob-based detector deliberately leaves them unmapped — unlike
    // the path detector, which resolves them. Excluding them here preserves
    // the historical `None` result.
    if matches!(ext, "c" | "h" | "hh") {
        return None;
    }
    extension_to_language(ext)
}

/// Fallback for `language_from_tags`: scan a rule's `file_patterns` and return
/// the language if every parseable language-bearing pattern resolves to the
/// same one. Mixed-language or universal pattern lists return None.
pub fn language_from_file_patterns(file_patterns_json: Option<&str>) -> Option<String> {
    let raw = file_patterns_json?.trim();
    if raw.is_empty() {
        return None;
    }
    let patterns: Vec<String> = serde_json::from_str(raw).ok()?;
    let mut seen: Option<&'static str> = None;
    for p in &patterns {
        if let Some(lang) = language_from_pattern(p) {
            match seen {
                None => seen = Some(lang),
                Some(existing) if existing == lang => {}
                Some(_) => return None,
            }
        }
    }
    seen.map(String::from)
}

pub fn repo_scope_from_source_repo(source_repo: Option<&str>) -> Option<String> {
    crate::infra::git::normalize_canonical_repo_scope(source_repo?)
}

impl From<RuleRow> for RuleDocument {
    fn from(r: RuleRow) -> Self {
        let language = language_from_tags(&r.tags)
            .or_else(|| language_from_file_patterns(r.file_patterns.as_deref()));
        let repo_scope = repo_scope_from_source_repo(r.source_repo.as_deref());
        // Include source repo attribution in indexed content so displayed rule
        // bodies can cite it and repo-specific queries get a small embedding
        // bias. Universal rules omit the line.
        let content = match repo_scope.as_deref() {
            Some(scope) => format!(
                "Rule ID: {}\nRule Name: {}\nType: {}\nSource: {}\nTags: {}\n\n{}",
                r.id, r.name, r.r#type, scope, r.tags, r.description
            ),
            None => format!(
                "Rule ID: {}\nRule Name: {}\nType: {}\nTags: {}\n\n{}",
                r.id, r.name, r.r#type, r.tags, r.description
            ),
        };
        Self {
            skill_id: r.id,
            title: r.name,
            content,
            confidence: r.confidence_score,
            file_patterns: r.file_patterns,
            language,
            repo_scope,
        }
    }
}

pub async fn load_rules_from_db(pool: &SqlitePool) -> Result<Vec<RuleDocument>, CoreError> {
    load_rules_from_db_for_engine(pool, None).await
}

pub async fn load_soft_preferences_for_engine(
    pool: &SqlitePool,
    engine: Option<&str>,
    repo_scopes: &[String],
    limit: usize,
) -> Result<Vec<SoftPreferenceDocument>, CoreError> {
    let enabled_clause = match engine {
        Some("codex") => "AND enabled_for_codex = 1",
        Some("claude") => "AND enabled_for_claude = 1",
        Some("gemini") => "AND enabled_for_gemini = 1",
        Some("cursor") => "AND enabled_for_cursor = 1",
        _ => "",
    };
    let mut sql = format!(
        "SELECT id, name, description, source_repo FROM skills \
         WHERE status = 'active' AND type = 'soft_preference' {enabled_clause} \
           AND (source_repo IS NULL OR TRIM(source_repo) = ''"
    );
    if !repo_scopes.is_empty() {
        let placeholders = std::iter::repeat_n("?", repo_scopes.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(" OR LOWER(source_repo) IN (");
        sql.push_str(&placeholders);
        sql.push(')');
    }
    sql.push_str(") ORDER BY updated_at DESC, installed_at DESC, id ASC LIMIT ?");

    let mut query = sqlx::query_as::<_, (String, String, String, Option<String>)>(&sql);
    for repo_scope in repo_scopes {
        query = query.bind(repo_scope.to_ascii_lowercase());
    }
    query = query.bind(i64::try_from(limit).unwrap_or(i64::MAX));

    let rows = query.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(
            |(skill_id, title, body, source_repo)| SoftPreferenceDocument {
                skill_id,
                title,
                body,
                source_repo,
            },
        )
        .collect())
}

pub async fn load_rule_index_state(pool: &SqlitePool) -> Result<RuleIndexState, CoreError> {
    // Pending candidates are not served, so the index-state hash must ignore
    // them too; otherwise a pending insert would invalidate the rule index
    // without changing any served document.
    let (rule_count, max_updated_at): (i64, Option<String>) = sqlx::query_as(
        "SELECT COUNT(*) AS rule_count, MAX(updated_at) AS max_updated_at \
         FROM skills WHERE status = 'active' AND type != 'soft_preference'",
    )
    .fetch_one(pool)
    .await?;
    Ok(RuleIndexState {
        rule_count,
        max_updated_at,
        embedding_profile: crate::context::embedding::active_embedding_profile().await,
        // Base state describes the whole active corpus, so it carries no scope
        // signature; the orchestrator fills this in after filtering for scope.
        scope_signature: None,
    })
}

pub async fn load_rules_from_db_for_engine(
    pool: &SqlitePool,
    engine: Option<&str>,
) -> Result<Vec<RuleDocument>, CoreError> {
    // Pending candidates (e.g. ingested agent memory) MUST NOT surface here —
    // they exist for team review on the dashboard, not for injection into
    // agent context.
    let rows = match engine {
        Some("codex") => {
            sqlx::query_as::<_, RuleRow>(
                "SELECT id, name, description, type as \"type\", tags, confidence_score, \
             file_patterns, source_repo FROM skills \
             WHERE enabled_for_codex = 1 AND status = 'active' AND type != 'soft_preference'",
            )
            .fetch_all(pool)
            .await?
        }
        Some("claude") => {
            sqlx::query_as::<_, RuleRow>(
                "SELECT id, name, description, type as \"type\", tags, confidence_score, \
             file_patterns, source_repo FROM skills \
             WHERE enabled_for_claude = 1 AND status = 'active' AND type != 'soft_preference'",
            )
            .fetch_all(pool)
            .await?
        }
        Some("gemini") => {
            sqlx::query_as::<_, RuleRow>(
                "SELECT id, name, description, type as \"type\", tags, confidence_score, \
             file_patterns, source_repo FROM skills \
             WHERE enabled_for_gemini = 1 AND status = 'active' AND type != 'soft_preference'",
            )
            .fetch_all(pool)
            .await?
        }
        Some("cursor") => {
            sqlx::query_as::<_, RuleRow>(
                "SELECT id, name, description, type as \"type\", tags, confidence_score, \
             file_patterns, source_repo FROM skills \
             WHERE enabled_for_cursor = 1 AND status = 'active' AND type != 'soft_preference'",
            )
            .fetch_all(pool)
            .await?
        }
        _ => {
            sqlx::query_as::<_, RuleRow>(
                "SELECT id, name, description, type as \"type\", tags, confidence_score, \
             file_patterns, source_repo FROM skills \
             WHERE status = 'active' AND type != 'soft_preference'",
            )
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows.into_iter().map(RuleDocument::from).collect())
}

/// Build a `skill_id -> confidence_score` map used by retrieval to weight RRF
/// scores so a high-confidence rule outranks a fresh capture with the same
/// lexical signal. Skips heavy text columns so it stays cheap on every hook.
///
/// Confidence semantics (`skills.confidence_score` defaults):
///   - manual / cloud-extracted base: 0.7
///   - conversation-channel base: 0.6 (fidelity discount on agent transcription)
///   - dedup-bump: +0.05 per re-capture
///   - feedback dismiss: -0.10
pub async fn load_rule_confidence_map(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, f64>, CoreError> {
    let rows = sqlx::query!("SELECT id, confidence_score FROM skills WHERE status = 'active'")
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.id, row.confidence_score))
        .collect())
}

/// Ranking metadata shared by CLI search/recall and MCP runtime recall. One
/// loader for confidence and age maps so a callsite can't apply confidence
/// boosts while skipping the half-life decay input.
#[derive(Debug, Clone, Default)]
pub struct RuleRankingInputs {
    pub confidence_map: Option<std::collections::HashMap<String, f64>>,
    pub age_days_map: Option<std::collections::HashMap<String, f32>>,
    /// Per-rule apply-clean rate among accepted fixes from recorded fix
    /// outcomes, folded into ranking as a small, capped, reward-only multiplier
    /// that is orthogonal to `confidence_map` (see retrieval's
    /// `apply_effectiveness_weight` and `fix_outcomes::rule_effectiveness_map`).
    /// `None` when unavailable — ranking simply degrades to neutral, never errors.
    pub effectiveness_map: Option<std::collections::HashMap<String, f64>>,
}

pub async fn load_rule_ranking_inputs(pool: &SqlitePool) -> RuleRankingInputs {
    RuleRankingInputs {
        confidence_map: load_rule_confidence_map(pool).await.ok(),
        age_days_map: load_rule_age_days_map(pool).await.ok(),
        effectiveness_map: crate::observability::fix_outcomes::rule_effectiveness_map(
            pool,
            crate::observability::fix_outcomes::EFFECTIVENESS_MIN_SAMPLES,
        )
        .await
        .ok(),
    }
}

/// Build `skill_id -> age_in_days` map for the half-life decay applied at
/// retrieval time. Age uses `created_at`, falling back to `updated_at`. Skills
/// with neither set are omitted; retrieval treats absence as `age_days = 0`.
///
/// Uses runtime `sqlx::query()` so this optional metadata doesn't depend on
/// offline SQLx metadata.
pub async fn load_rule_age_days_map(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, f32>, CoreError> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT id, COALESCE(created_at, updated_at) AS ts \
         FROM skills WHERE status = 'active'",
    )
    .fetch_all(pool)
    .await?;
    let now = chrono::Utc::now();
    let mut out = std::collections::HashMap::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get("id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let ts: Option<String> = row.try_get("ts").ok();
        let Some(ts) = ts else { continue };
        // SQLite stores timestamps as ISO-8601 strings. Try RFC3339 (the
        // canonical write path) first, then a few common SQLite shapes. A parse
        // failure omits the entry; retrieval defaults age to 0, so a malformed
        // timestamp degrades to "no decay" rather than mis-aging the rule.
        let parsed = chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .ok()
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|n| n.and_utc())
            })
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%d %H:%M:%S%.f")
                    .ok()
                    .map(|n| n.and_utc())
            })
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%dT%H:%M:%S%.f")
                    .ok()
                    .map(|n| n.and_utc())
            });
        if let Some(created) = parsed {
            let age_days = (now - created).num_seconds().max(0) as f32 / 86_400.0;
            out.insert(id, age_days);
        }
    }
    Ok(out)
}

/// Load few-shot code examples for a given skill
pub async fn load_rule_examples(
    pool: &SqlitePool,
    skill_id: &str,
) -> Result<Vec<RuleExample>, CoreError> {
    let rows = sqlx::query_as!(
        RuleExampleRow,
        "SELECT id, skill_id, bad_code, good_code, description, source \
         FROM rule_examples WHERE skill_id = ?1 ORDER BY created_at DESC LIMIT 3",
        skill_id
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(RuleExample::from).collect())
}

/// Load examples for multiple skills in one batch
pub async fn load_rule_examples_batch(
    pool: &SqlitePool,
    skill_ids: &[String],
) -> Result<std::collections::HashMap<String, Vec<RuleExample>>, CoreError> {
    if skill_ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let ids_json = serde_json::to_string(skill_ids)
        .map_err(|e| CoreError::Internal(format!("serialize skill_ids: {e}")))?;
    let rows = sqlx::query_as!(
        RuleExampleRow,
        "SELECT id, skill_id, bad_code, good_code, description, source \
         FROM rule_examples \
         WHERE skill_id IN (SELECT value FROM json_each(?1)) \
         ORDER BY created_at DESC",
        ids_json,
    )
    .fetch_all(pool)
    .await?;

    let mut map: std::collections::HashMap<String, Vec<RuleExample>> =
        std::collections::HashMap::new();
    for row in rows {
        let skill_id = row.skill_id.clone();
        let example = RuleExample::from(row);
        map.entry(skill_id).or_default().push(example);
    }
    for examples in map.values_mut() {
        examples.truncate(3);
    }
    Ok(map)
}

#[derive(Debug, Clone)]
pub struct RuleExample {
    pub id: String,
    pub skill_id: String,
    pub bad_code: String,
    pub good_code: String,
    pub description: Option<String>,
    pub source: String,
}

#[derive(sqlx::FromRow)]
struct RuleExampleRow {
    id: String,
    skill_id: String,
    bad_code: String,
    good_code: String,
    description: Option<String>,
    source: String,
}

impl From<RuleExampleRow> for RuleExample {
    fn from(r: RuleExampleRow) -> Self {
        Self {
            id: r.id,
            skill_id: r.skill_id,
            bad_code: r.bad_code,
            good_code: r.good_code,
            description: r.description,
            source: r.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_from_tags_singleton_downweighted() {
        let c = confidence_from_tags(r#"["auto-from-extractions","cluster-size:1"]"#).unwrap();
        assert!((c - 0.55).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn confidence_from_tags_large_cluster_strongest() {
        let c = confidence_from_tags(r#"["cluster-size:8","severity:warning"]"#).unwrap();
        assert!((c - 0.9).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn confidence_from_tags_severity_error_boosts() {
        let c = confidence_from_tags(r#"["cluster-size:3","severity:error"]"#).unwrap();
        assert!((c - 0.85).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn confidence_from_tags_severity_info_dampens() {
        let c = confidence_from_tags(r#"["cluster-size:1","severity:info"]"#).unwrap();
        assert!((c - 0.50).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn confidence_from_tags_missing_evidence_returns_none() {
        assert_eq!(
            confidence_from_tags(r#"["auto-from-extractions","origin:review-extraction"]"#),
            None
        );
        assert_eq!(confidence_from_tags("[]"), None);
        assert_eq!(confidence_from_tags(""), None);
        assert_eq!(confidence_from_tags("not-json"), None);
    }

    #[test]
    fn confidence_from_tags_malformed_cluster_size_is_conservative() {
        let c = confidence_from_tags(r#"["cluster-size:oops"]"#).unwrap();
        assert!((c - 0.4).abs() < 1e-9, "got {c}");

        let c = confidence_from_tags(r#"["cluster-size:oops","severity:error"]"#).unwrap();
        assert!((c - 0.45).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn language_from_tags_table() {
        let cases: &[(&str, Option<&str>)] = &[
            (r#"["async", "rust", "concurrency"]"#, Some("rust")),
            (r#"["typescript", "react"]"#, Some("typescript")),
            // Normalised aliases.
            (r#"["c++"]"#, Some("cpp")),
            (r#"["C#"]"#, Some("csharp")),
            // No known language tag — fall through to None.
            ("[]", None),
            ("", None),
            ("not-json", None),
            (r#"["lint", "performance"]"#, None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                language_from_tags(input).as_deref(),
                *expected,
                "input: {input}"
            );
        }
    }

    #[test]
    fn language_from_file_patterns_resolves_single_language() {
        assert_eq!(
            language_from_file_patterns(Some(r#"["**/*.rs"]"#)).as_deref(),
            Some("rust")
        );
        assert_eq!(
            language_from_file_patterns(Some(r#"["**/*.ts","**/*.tsx"]"#)).as_deref(),
            Some("typescript")
        );
        assert_eq!(
            language_from_file_patterns(Some(r#"["src/**/*.go","tests/**/*.go"]"#)).as_deref(),
            Some("go")
        );
    }

    #[test]
    fn language_from_file_patterns_returns_none_for_mixed_or_universal() {
        // Mixed languages → can't pick one without guessing.
        assert_eq!(
            language_from_file_patterns(Some(r#"["**/*.rs","**/*.go"]"#)),
            None
        );
        // Universal pattern → applies everywhere.
        assert_eq!(language_from_file_patterns(Some(r#"["**/*"]"#)), None);
        // Test glob without language extension.
        assert_eq!(language_from_file_patterns(Some(r#"["**/*test*"]"#)), None);
    }

    #[test]
    fn language_from_file_patterns_handles_missing_or_empty_input() {
        assert_eq!(language_from_file_patterns(None), None);
        assert_eq!(language_from_file_patterns(Some("")), None);
        assert_eq!(language_from_file_patterns(Some("[]")), None);
        assert_eq!(language_from_file_patterns(Some("not-json")), None);
    }

    #[test]
    fn repo_scope_uses_canonical_source_repo_only() {
        assert_eq!(
            repo_scope_from_source_repo(Some("vitejs/vite")).as_deref(),
            Some("vitejs/vite")
        );
        assert_eq!(
            repo_scope_from_source_repo(Some("gitlab.com/group/sub/project")).as_deref(),
            Some("gitlab.com/group/sub/project")
        );
        assert_eq!(
            repo_scope_from_source_repo(Some("gitlab.corp.example/group/project")).as_deref(),
            Some("gitlab.corp.example/group/project")
        );
        assert!(repo_scope_from_source_repo(None).is_none());
        assert!(repo_scope_from_source_repo(Some("vitejs")).is_none());
        assert!(repo_scope_from_source_repo(Some(" /vite")).is_none());
        assert!(repo_scope_from_source_repo(Some("github.com/owner/repo")).is_none());
        assert!(repo_scope_from_source_repo(Some("group/sub/project")).is_none());
    }

    #[test]
    fn scope_signature_depends_only_on_membership() {
        // Same set, different iteration order → same sig. Otherwise recall
        // would spuriously invalidate freshness and re-embed the whole corpus.
        assert_eq!(
            scope_signature_from_skill_ids(["a", "b", "c"]),
            scope_signature_from_skill_ids(["c", "a", "b"]),
        );
        // Dedup: repeated ids do not change the signature (membership-only).
        assert_eq!(
            scope_signature_from_skill_ids(["a", "a", "b"]),
            scope_signature_from_skill_ids(["a", "b"]),
        );
        // Empty set → scope-agnostic `None` (freshness check ignores scope).
        assert_eq!(scope_signature_from_skill_ids(Vec::<&str>::new()), None);
        // A genuine membership change MUST change the signature — this is what
        // lets the index-freshness check catch a scope change even when the
        // rule count is unchanged.
        assert_ne!(
            scope_signature_from_skill_ids(["a", "b"]),
            scope_signature_from_skill_ids(["a", "c"]),
        );
    }

    #[test]
    fn scope_signature_length_delimits_to_avoid_collision() {
        // Without the NUL length-delimiter between ids, ["ab","c"] and
        // ["a","bc"] would hash the same concatenated bytes ("abc") and
        // collide — a real scope change would then be silently missed and
        // stale chunks served.
        assert_ne!(
            scope_signature_from_skill_ids(["ab", "c"]),
            scope_signature_from_skill_ids(["a", "bc"]),
        );
    }
}
