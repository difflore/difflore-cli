//! "What the AI has learned" preview for `difflore doctor`.
//!
//! Renders a compact section under the readiness table showing total rule
//! count, top source repositories, and the most recently learned rules.
//!
//! All DB queries are best-effort: any error returns an empty result and the
//! section silently shrinks, never surfacing a render-time DB hiccup as a
//! user-visible failure.

use crate::commands::util::format_recall_edit_proof_breakdown;
use crate::style;
use difflore_core::cloud::observations::ObservationUploadIssue;
use std::collections::{BTreeMap, HashMap};

type ProvenRow = (Option<String>, String, Option<String>, i64, Option<String>);

/// One entry in the "top repos by rule count" line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoCount {
    pub(crate) repo: String,
    pub(crate) count: i64,
}

/// One entry in the "most recently learned rules" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecentRule {
    pub(crate) name: String,
    pub(crate) source_repo: Option<String>,
}

/// One entry in the "rules with accepted local fixes" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvenRule {
    pub(crate) id: Option<String>,
    pub(crate) name: String,
    pub(crate) source_repo: Option<String>,
    pub(crate) accepted_count: i64,
    pub(crate) accepted_fix_proofs: i64,
    pub(crate) accepted_hook_outcomes: i64,
    pub(crate) accepted_hook_outcomes_linked_to_prior_recall: i64,
    pub(crate) accepted_hook_outcomes_linked_to_rule_recall: i64,
    pub(crate) accepted_hook_outcomes_linked_to_mcp_rule_serve: i64,
    pub(crate) accepted_hook_outcomes_linked_to_edit_attribution: i64,
    pub(crate) sample_file: Option<String>,
}

#[derive(Debug, Clone)]
struct ProvenRuleCandidate {
    id: String,
    name: String,
    source_repo: Option<String>,
    accepted_fix_proofs: i64,
    accepted_hook_outcomes: i64,
    accepted_hook_outcomes_linked_to_prior_recall: i64,
    accepted_hook_outcomes_linked_to_rule_recall: i64,
    accepted_hook_outcomes_linked_to_mcp_rule_serve: i64,
    accepted_hook_outcomes_linked_to_edit_attribution: i64,
    sample_file: Option<String>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct RuleMetadataRow {
    rule_id: String,
    name: String,
    source_repo: Option<String>,
}

/// Local proof that surfaced rules are making it into agent replies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentCitationProof {
    pub(crate) actual_citations: i64,
    pub(crate) rule_fires: i64,
    pub(crate) pending_uploads: i64,
    pub(crate) pending_upload_issue: Option<ObservationUploadIssue>,
}

/// Read-only snapshot of memory state used to render the section.
#[derive(Debug, Clone, Default)]
pub(crate) struct MemorySnapshot {
    pub(crate) total_rules: i64,
    pub(crate) top_repos: Vec<RepoCount>,
    pub(crate) recent: Vec<RecentRule>,
    pub(crate) proven: Vec<ProvenRule>,
    pub(crate) agent_citation: Option<AgentCitationProof>,
}

/// Load the snapshot from the live project DB. Any DB error collapses to
/// defaults so the doctor table still renders.
#[cfg(test)]
pub(crate) async fn load(pool: &difflore_core::SqlitePool) -> MemorySnapshot {
    let total_rules = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM skills WHERE COALESCE(status, 'active') = 'active'"#,
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);

    let repo_aliases = current_repo_aliases();
    let top_repos = fetch_top_repos(pool).await;
    let recent = fetch_recent(pool).await;
    let proven = fetch_proven(pool, &repo_aliases).await;
    let agent_citation = fetch_agent_citation_proof().await;
    MemorySnapshot {
        total_rules,
        top_repos,
        recent,
        proven,
        agent_citation,
    }
}

/// Load the memory snapshot scoped to the current repo / upstream aliases.
/// Unlike [`load`], this never falls back to other repos, so doctor shows no
/// unrelated proof once a repo scope is established.
pub(crate) async fn load_for_repo(
    pool: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> MemorySnapshot {
    let normalized = normalize_repo_aliases(repo_aliases);
    if normalized.is_empty() {
        return MemorySnapshot::default();
    }

    let top_repos = fetch_top_repos_for(pool, Some(&normalized)).await;
    let total_rules = top_repos.iter().map(|repo| repo.count).sum();
    let recent = fetch_recent_for(pool, Some(&normalized)).await;
    let proven = fetch_proven_scoped(pool, &normalized).await;
    MemorySnapshot {
        total_rules,
        top_repos,
        recent,
        proven,
        agent_citation: None,
    }
}

fn normalize_repo_aliases(repo_aliases: &[String]) -> Vec<String> {
    repo_aliases
        .iter()
        .map(|repo| repo.trim().trim_end_matches(".git").to_ascii_lowercase())
        .filter(|repo| !repo.is_empty())
        .collect()
}

#[cfg(test)]
async fn fetch_top_repos(pool: &difflore_core::SqlitePool) -> Vec<RepoCount> {
    fetch_top_repos_for(pool, None).await
}

async fn fetch_top_repos_for(
    pool: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
) -> Vec<RepoCount> {
    let repo_filter = normalized_repos
        .filter(|repos| !repos.is_empty())
        .map(|repos| {
            let placeholders = std::iter::repeat_n("?", repos.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND LOWER(source_repo) IN ({placeholders})")
        })
        .unwrap_or_default();
    let sql = format!(
        "SELECT source_repo, COUNT(*) FROM skills \
         WHERE source_repo IS NOT NULL AND source_repo != '' \
           AND COALESCE(status, 'active') = 'active' \
           {repo_filter} \
         GROUP BY source_repo \
         ORDER BY COUNT(*) DESC \
         LIMIT 5"
    );
    let mut query = sqlx::query_as::<_, (String, i64)>(&sql);
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }
    let rows: Result<Vec<(String, i64)>, sqlx::Error> = query.fetch_all(pool).await;
    rows.unwrap_or_default()
        .into_iter()
        .map(|(repo, count)| RepoCount { repo, count })
        .collect()
}

#[cfg(test)]
async fn fetch_recent(pool: &difflore_core::SqlitePool) -> Vec<RecentRule> {
    fetch_recent_for(pool, None).await
}

async fn fetch_recent_for(
    pool: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
) -> Vec<RecentRule> {
    let repo_filter = normalized_repos
        .filter(|repos| !repos.is_empty())
        .map(|repos| {
            let placeholders = std::iter::repeat_n("?", repos.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND LOWER(COALESCE(source_repo, '')) IN ({placeholders})")
        })
        .unwrap_or_default();
    let sql = format!(
        "SELECT name, source_repo FROM skills \
         WHERE COALESCE(status, 'active') = 'active' \
           {repo_filter} \
         ORDER BY installed_at DESC \
         LIMIT 3"
    );
    let mut query = sqlx::query_as::<_, (String, Option<String>)>(&sql);
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }
    let rows: Result<Vec<(String, Option<String>)>, sqlx::Error> = query.fetch_all(pool).await;
    rows.unwrap_or_default()
        .into_iter()
        .map(|(name, source_repo)| RecentRule { name, source_repo })
        .collect()
}

#[cfg(test)]
fn current_repo_aliases() -> Vec<String> {
    let root = difflore_core::infra::db::current_project_root();
    difflore_core::infra::git::detect_github_repo_full_names(&root.to_string_lossy())
}

#[cfg(test)]
async fn fetch_proven(
    pool: &difflore_core::SqlitePool,
    repo_aliases: &[String],
) -> Vec<ProvenRule> {
    let candidates: Vec<String> = repo_aliases
        .iter()
        .map(|repo| repo.trim().to_ascii_lowercase())
        .filter(|repo| !repo.is_empty())
        .collect();
    let hook_summaries = fetch_hook_accepted_rule_summaries().await;

    if !candidates.is_empty() {
        let scoped =
            fetch_proven_with_hook_summaries(pool, Some(&candidates), &hook_summaries).await;
        if !scoped.is_empty() {
            return scoped;
        }
    }

    fetch_proven_with_hook_summaries(pool, None, &hook_summaries).await
}

async fn fetch_proven_scoped(
    pool: &difflore_core::SqlitePool,
    normalized_repo_aliases: &[String],
) -> Vec<ProvenRule> {
    if normalized_repo_aliases.is_empty() {
        return Vec::new();
    }
    // Hook/agent outcomes carry no canonical target repo, so a repo-scoped
    // snapshot fails closed and shows only signed local fix proof from rules
    // whose source_repo matches this repo/upstream alias.
    let hook_summaries = Vec::new();
    fetch_proven_with_hook_summaries(pool, Some(normalized_repo_aliases), &hook_summaries).await
}

#[cfg(test)]
async fn fetch_hook_accepted_rule_summaries()
-> Vec<difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary> {
    #[cfg(test)]
    {
        Vec::new()
    }
    #[cfg(not(test))]
    {
        match difflore_core::cloud::observations::ObservationEmitter::open_default().await {
            Ok(emitter) => emitter
                .accepted_fix_outcome_rule_summaries(30, 7)
                .await
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }
}

async fn fetch_proven_with_hook_summaries(
    pool: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
    hook_summaries: &[difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary],
) -> Vec<ProvenRule> {
    let mut candidates: BTreeMap<String, ProvenRuleCandidate> = BTreeMap::new();

    for (id, name, source_repo, accepted_fix_proofs, sample_file) in
        fetch_signed_proven_rows(pool, normalized_repos).await
    {
        let Some(id) = id.filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        candidates.insert(
            id.clone(),
            ProvenRuleCandidate {
                id,
                name,
                source_repo,
                accepted_fix_proofs,
                accepted_hook_outcomes: 0,
                accepted_hook_outcomes_linked_to_prior_recall: 0,
                accepted_hook_outcomes_linked_to_rule_recall: 0,
                accepted_hook_outcomes_linked_to_mcp_rule_serve: 0,
                accepted_hook_outcomes_linked_to_edit_attribution: 0,
                sample_file,
            },
        );
    }

    let hook_ids: Vec<String> = hook_summaries
        .iter()
        .map(|summary| summary.rule_id.clone())
        .collect();
    let metadata = fetch_rule_metadata_for_ids(pool, &hook_ids, normalized_repos).await;
    for summary in hook_summaries {
        let Some(rule) = metadata.get(&summary.rule_id) else {
            continue;
        };
        let candidate = candidates
            .entry(summary.rule_id.clone())
            .or_insert_with(|| ProvenRuleCandidate {
                id: summary.rule_id.clone(),
                name: rule.name.clone(),
                source_repo: rule.source_repo.clone(),
                accepted_fix_proofs: 0,
                accepted_hook_outcomes: 0,
                accepted_hook_outcomes_linked_to_prior_recall: 0,
                accepted_hook_outcomes_linked_to_rule_recall: 0,
                accepted_hook_outcomes_linked_to_mcp_rule_serve: 0,
                accepted_hook_outcomes_linked_to_edit_attribution: 0,
                sample_file: None,
            });
        candidate.accepted_hook_outcomes += summary.accepted_outcomes;
        candidate.accepted_hook_outcomes_linked_to_prior_recall += summary.linked_to_prior_recall;
        candidate.accepted_hook_outcomes_linked_to_rule_recall += summary.linked_to_rule_recall;
        candidate.accepted_hook_outcomes_linked_to_mcp_rule_serve +=
            summary.linked_to_mcp_rule_serve;
        candidate.accepted_hook_outcomes_linked_to_edit_attribution +=
            summary.linked_to_edit_attribution;
        if candidate
            .sample_file
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
            && let Some(file) = summary
                .sample_file
                .as_deref()
                .map(str::trim)
                .filter(|file| !file.is_empty())
        {
            candidate.sample_file = Some(file.to_owned());
        }
    }

    let mut out: Vec<ProvenRule> = candidates
        .into_values()
        .filter(|candidate| candidate.accepted_fix_proofs + candidate.accepted_hook_outcomes > 0)
        .map(|candidate| ProvenRule {
            id: Some(candidate.id),
            name: candidate.name,
            source_repo: candidate.source_repo,
            accepted_count: candidate.accepted_fix_proofs + candidate.accepted_hook_outcomes,
            accepted_fix_proofs: candidate.accepted_fix_proofs,
            accepted_hook_outcomes: candidate.accepted_hook_outcomes,
            accepted_hook_outcomes_linked_to_prior_recall: candidate
                .accepted_hook_outcomes_linked_to_prior_recall,
            accepted_hook_outcomes_linked_to_rule_recall: candidate
                .accepted_hook_outcomes_linked_to_rule_recall,
            accepted_hook_outcomes_linked_to_mcp_rule_serve: candidate
                .accepted_hook_outcomes_linked_to_mcp_rule_serve,
            accepted_hook_outcomes_linked_to_edit_attribution: candidate
                .accepted_hook_outcomes_linked_to_edit_attribution,
            sample_file: candidate.sample_file,
        })
        .collect();

    out.sort_by(|a, b| {
        b.accepted_count
            .cmp(&a.accepted_count)
            .then(
                b.accepted_hook_outcomes_linked_to_prior_recall
                    .cmp(&a.accepted_hook_outcomes_linked_to_prior_recall),
            )
            .then(b.accepted_fix_proofs.cmp(&a.accepted_fix_proofs))
            .then(a.name.cmp(&b.name))
    });
    out.truncate(2);
    out
}

async fn fetch_signed_proven_rows(
    pool: &difflore_core::SqlitePool,
    normalized_repos: Option<&[String]>,
) -> Vec<ProvenRow> {
    let repo_filter = normalized_repos
        .filter(|repos| !repos.is_empty())
        .map(|repos| {
            let placeholders = std::iter::repeat_n("?", repos.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND LOWER(s.source_repo) IN ({placeholders})")
        })
        .unwrap_or_default();
    let sql = format!(
        "SELECT s.id AS rule_id, \
                COALESCE(NULLIF(s.name, ''), f.rule_name) AS name, \
                s.source_repo AS source_repo, \
                COUNT(*) AS accepted_count, \
                MAX(NULLIF(f.file_path, '')) AS sample_file \
         FROM fix_outcomes f \
         LEFT JOIN skills s ON s.id = f.rule_id \
         WHERE f.accepted = 1 AND f.applied_ok = 1 \
           {repo_filter} \
         GROUP BY s.id, COALESCE(NULLIF(s.name, ''), f.rule_name), s.source_repo \
         ORDER BY COUNT(*) DESC, MAX(f.created_at) DESC \
         LIMIT 20"
    );
    let mut query = sqlx::query_as::<_, ProvenRow>(&sql);
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }
    query.fetch_all(pool).await.unwrap_or_default()
}

async fn fetch_rule_metadata_for_ids(
    pool: &difflore_core::SqlitePool,
    rule_ids: &[String],
    normalized_repos: Option<&[String]>,
) -> HashMap<String, RuleMetadataRow> {
    let ids: Vec<&str> = rule_ids
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        return HashMap::new();
    }

    let id_placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    let repo_filter = normalized_repos
        .filter(|repos| !repos.is_empty())
        .map(|repos| {
            let placeholders = std::iter::repeat_n("?", repos.len())
                .collect::<Vec<_>>()
                .join(", ");
            format!("AND LOWER(COALESCE(source_repo, '')) IN ({placeholders})")
        })
        .unwrap_or_default();
    let sql = format!(
        "SELECT id AS rule_id, \
                COALESCE(NULLIF(name, ''), id) AS name, \
                source_repo AS source_repo \
         FROM skills \
         WHERE id IN ({id_placeholders}) \
           AND COALESCE(status, 'active') = 'active' \
           {repo_filter}"
    );
    let mut query = sqlx::query_as::<_, RuleMetadataRow>(&sql);
    for id in ids {
        query = query.bind(id);
    }
    if let Some(repos) = normalized_repos {
        for repo in repos {
            query = query.bind(repo);
        }
    }

    query
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.rule_id.clone(), row))
        .collect()
}

fn recall_command_for_proven(rule: &ProvenRule) -> String {
    let mut cmd = format!("difflore recall {}", quote_arg(&rule.name));
    if let Some(file) = rule
        .sample_file
        .as_deref()
        .filter(|file| !file.trim().is_empty())
    {
        cmd.push_str(" --file ");
        cmd.push_str(&quote_arg(file));
    }
    cmd.push_str(" --top-k 3");
    cmd
}

fn quote_arg(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

#[cfg(test)]
async fn fetch_agent_citation_proof() -> Option<AgentCitationProof> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        let summary = difflore_core::cloud::observations::actual_citation_summary_default(7)
            .await
            .ok()?;
        if summary.actual_citations == 0 && summary.rule_fires == 0 {
            return None;
        }
        Some(AgentCitationProof {
            actual_citations: summary.actual_citations,
            rule_fires: summary.rule_fires,
            pending_uploads: summary.pending_uploads,
            pending_upload_issue: summary.pending_upload_issue,
        })
    }
}

fn agent_citation_line(proof: &AgentCitationProof) -> String {
    let mut line = format!(
        "{} actual citation{} · {} memory fire{} in 7d",
        proof.actual_citations,
        if proof.actual_citations == 1 { "" } else { "s" },
        proof.rule_fires,
        if proof.rule_fires == 1 { "" } else { "s" },
    );
    if proof.pending_uploads > 0 {
        line.push_str(&format!(
            " · {} pending upload{}",
            proof.pending_uploads,
            if proof.pending_uploads == 1 { "" } else { "s" },
        ));
    }
    line
}

fn agent_citation_recovery_line(proof: &AgentCitationProof) -> Option<String> {
    if proof.pending_uploads == 0 {
        return None;
    }
    let message = match proof.pending_upload_issue {
        Some(ObservationUploadIssue::MissingCloudScope) => format!(
            "{} {}",
            style::pewter("activity queued safely; refresh login once:"),
            style::cmd("difflore cloud login")
        ),
        Some(ObservationUploadIssue::RateLimited) => {
            style::pewter("cloud rate limit hit; uploads will retry automatically").to_string()
        }
        Some(ObservationUploadIssue::InvalidBatch) => {
            style::pewter("cloud rejected these activity uploads").to_string()
        }
        Some(ObservationUploadIssue::ServerRejected) => format!(
            "{} {}",
            style::pewter("inspect activity upload rejection:"),
            style::cmd("difflore doctor --report")
        ),
        Some(ObservationUploadIssue::Unknown) | None => format!(
            "{} {}",
            style::pewter("if uploads stay pending:"),
            style::cmd("difflore doctor --report")
        ),
    };
    Some(message)
}

/// Render the snapshot, returning an empty string for an empty corpus so the
/// caller can append unconditionally without a hollow heading.
pub(crate) fn render(snapshot: &MemorySnapshot) -> String {
    if snapshot.total_rules == 0 {
        return String::new();
    }
    // 10-char label column to match the denser `init` memory block, not the
    // doctor table's 17-char width.
    const LABEL_W: usize = 10;
    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("  {}\n", style::pewter("Memory snapshot")));

    // Up to 3 repos inline; collapse the rest into `+N more`.
    let repos_line = if snapshot.top_repos.is_empty() {
        style::pewter(&format!(
            "{} rule{} · no source_repo set",
            snapshot.total_rules,
            if snapshot.total_rules == 1 { "" } else { "s" },
        ))
        .to_string()
    } else {
        let inline: Vec<String> = snapshot
            .top_repos
            .iter()
            .take(3)
            .map(|r| format!("{} ({})", r.repo, r.count))
            .collect();
        let mut line = inline.join(" · ");
        let extra = snapshot.top_repos.len().saturating_sub(3);
        if extra > 0 {
            line.push_str(&format!("  +{extra} more"));
        }
        // Repo names are values, not commands; render plain to match other
        // doctor row values rather than the blue command color.
        line
    };
    out.push_str(&format!(
        "  {:<width$} {}\n",
        style::pewter("repos"),
        repos_line,
        width = LABEL_W,
    ));

    // Recent rules: each prefixed with `·`, suffixed with the origin repo.
    if !snapshot.recent.is_empty() {
        for (i, rule) in snapshot.recent.iter().enumerate() {
            let label = if i == 0 { "newest" } else { "" };
            let suffix = rule.source_repo.as_deref().map_or_else(String::new, |r| {
                format!("  {}", style::pewter(&format!("\u{2190} from {r}")))
            });
            out.push_str(&format!(
                "  {:<width$} {} {}{suffix}\n",
                style::pewter(label),
                style::pewter("\u{00b7}"),
                rule.name,
                width = LABEL_W,
            ));
        }
    }
    if !snapshot.proven.is_empty() {
        for (i, rule) in snapshot.proven.iter().enumerate() {
            let label = if i == 0 { "proven" } else { "" };
            let suffix = rule.source_repo.as_deref().map_or_else(String::new, |r| {
                format!("  {}", style::pewter(&format!("\u{2190} from {r}")))
            });
            out.push_str(&format!(
                "  {:<width$} {} {}  {}{suffix}\n",
                style::pewter(label),
                style::pewter("\u{00b7}"),
                rule.name,
                style::pewter(&accepted_proof_label(rule)),
                width = LABEL_W,
            ));
            let try_label = if i == 0 { "try" } else { "" };
            out.push_str(&format!(
                "  {:<width$} {}\n",
                style::pewter(try_label),
                style::cmd(&recall_command_for_proven(rule)),
                width = LABEL_W,
            ));
        }
    }
    if let Some(proof) = &snapshot.agent_citation {
        out.push_str(&format!(
            "  {:<width$} {}\n",
            style::pewter("agent"),
            style::ident(&agent_citation_line(proof)),
            width = LABEL_W,
        ));
        if let Some(recovery) = agent_citation_recovery_line(proof) {
            out.push_str(&format!(
                "  {:<width$} {}\n",
                style::pewter("fix"),
                recovery,
                width = LABEL_W,
            ));
        }
    }
    out
}

fn accepted_proof_label(rule: &ProvenRule) -> String {
    if rule.accepted_hook_outcomes <= 0 {
        return format!(
            "{} accepted {}",
            rule.accepted_count,
            fix_noun(rule.accepted_count)
        );
    }

    let mut detail = Vec::new();
    if rule.accepted_fix_proofs > 0 {
        detail.push(format!(
            "{} signed local {}",
            rule.accepted_fix_proofs,
            fix_noun(rule.accepted_fix_proofs)
        ));
    }
    detail.push(format!(
        "{} agent/hook outcome{}",
        rule.accepted_hook_outcomes,
        if rule.accepted_hook_outcomes == 1 {
            ""
        } else {
            "s"
        }
    ));
    if rule.accepted_hook_outcomes_linked_to_prior_recall > 0 {
        detail.push(format!(
            "{} linked to prior memory recall{}",
            rule.accepted_hook_outcomes_linked_to_prior_recall,
            format_recall_edit_proof_breakdown(
                rule.accepted_hook_outcomes_linked_to_rule_recall,
                rule.accepted_hook_outcomes_linked_to_mcp_rule_serve,
                rule.accepted_hook_outcomes_linked_to_edit_attribution,
            )
        ));
    }

    format!(
        "{} accepted outcome{} ({})",
        rule.accepted_count,
        if rule.accepted_count == 1 { "" } else { "s" },
        detail.join(" + ")
    )
}

const fn fix_noun(count: i64) -> &'static str {
    if count == 1 { "fix" } else { "fixes" }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_pool() -> difflore_core::SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE skills (\
                id TEXT PRIMARY KEY, \
                name TEXT NOT NULL, \
                source_repo TEXT, \
                status TEXT, \
                installed_at TEXT NOT NULL DEFAULT '1970-01-01T00:00:00Z'\
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (\
                id TEXT PRIMARY KEY, \
                rule_id TEXT, \
                rule_name TEXT NOT NULL, \
                file_path TEXT, \
                diff_signature TEXT, \
                accepted INTEGER NOT NULL, \
                applied_ok INTEGER NOT NULL DEFAULT 0, \
                failed_reason TEXT, \
                created_at TEXT DEFAULT (datetime('now')) NOT NULL\
             )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn insert(
        pool: &difflore_core::SqlitePool,
        id: &str,
        name: &str,
        repo: Option<&str>,
        status: &str,
        installed_at: &str,
    ) {
        sqlx::query!(
            "INSERT INTO skills (id, name, source_repo, status, installed_at) \
             VALUES (?, ?, ?, ?, ?)",
            id,
            name,
            repo,
            status,
            installed_at,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn empty_corpus_yields_empty_render() {
        let pool = fresh_pool().await;
        let snap = load(&pool).await;
        assert_eq!(snap.total_rules, 0);
        assert!(snap.top_repos.is_empty());
        assert!(snap.recent.is_empty());
        assert!(snap.proven.is_empty());
        assert!(render(&snap).is_empty());
    }

    #[tokio::test]
    async fn happy_path_orders_repos_and_recent() {
        let pool = fresh_pool().await;
        // Two repos, gin has 3 active rules, vite has 2. One archived
        // rule should be ignored by both queries.
        insert(
            &pool,
            "g1",
            "Return 413 for body size limit errors",
            Some("gin-gonic/gin"),
            "active",
            "2026-05-04T10:00:00Z",
        )
        .await;
        insert(
            &pool,
            "g2",
            "Use defer for cleanup paths",
            Some("gin-gonic/gin"),
            "active",
            "2026-05-03T10:00:00Z",
        )
        .await;
        insert(
            &pool,
            "g3",
            "Validate route params early",
            Some("gin-gonic/gin"),
            "active",
            "2026-05-01T10:00:00Z",
        )
        .await;
        insert(
            &pool,
            "v1",
            "Avoid default exports in hot modules",
            Some("vitejs/vite"),
            "active",
            "2026-05-02T10:00:00Z",
        )
        .await;
        insert(
            &pool,
            "v2",
            "Lazy-load plugins",
            Some("vitejs/vite"),
            "active",
            "2026-04-28T10:00:00Z",
        )
        .await;
        insert(
            &pool,
            "x1",
            "Old archived rule",
            Some("acme/old"),
            "archived",
            "2026-05-04T11:00:00Z",
        )
        .await;

        let snap = load(&pool).await;
        assert_eq!(snap.total_rules, 5);
        assert_eq!(snap.top_repos.len(), 2);
        assert_eq!(snap.top_repos[0].repo, "gin-gonic/gin");
        assert_eq!(snap.top_repos[0].count, 3);
        assert_eq!(snap.top_repos[1].repo, "vitejs/vite");
        assert_eq!(snap.top_repos[1].count, 2);
        assert_eq!(snap.recent.len(), 3);
        assert_eq!(snap.recent[0].name, "Return 413 for body size limit errors");
        assert_eq!(snap.recent[0].source_repo.as_deref(), Some("gin-gonic/gin"));

        let rendered = render(&snap);
        assert!(rendered.contains("Memory snapshot"));
        assert!(rendered.contains("gin-gonic/gin (3)"));
        assert!(rendered.contains("vitejs/vite (2)"));
        assert!(rendered.contains("Return 413 for body size limit errors"));
        assert!(rendered.contains("\u{2190} from gin-gonic/gin"));
    }

    #[tokio::test]
    async fn load_for_repo_filters_snapshot_to_current_aliases() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            "router",
            "Pin Actions to commit SHAs",
            Some("tanstack/router"),
            "active",
            "2026-05-04T10:00:00Z",
        )
        .await;
        insert(
            &pool,
            "fastapi",
            "Use Mapping for headers",
            Some("fastapi/fastapi"),
            "active",
            "2026-05-05T10:00:00Z",
        )
        .await;
        sqlx::query(
            "INSERT INTO fix_outcomes \
             (id, rule_id, rule_name, file_path, accepted, applied_ok, created_at) \
             VALUES \
             ('r1', 'router', 'Pin Actions to commit SHAs', '.github/workflows/ci.yml', 1, 1, '2026-05-05T10:00:00Z'), \
             ('f1', 'fastapi', 'Use Mapping for headers', 'fastapi/applications.py', 1, 1, '2026-05-05T11:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let snap = load_for_repo(
            &pool,
            &["difflore-fixtures/router".into(), "tanstack/router".into()],
        )
        .await;

        assert_eq!(snap.total_rules, 1);
        assert_eq!(snap.top_repos[0].repo, "tanstack/router");
        assert_eq!(snap.recent.len(), 1);
        assert_eq!(snap.recent[0].name, "Pin Actions to commit SHAs");
        assert_eq!(snap.proven.len(), 1);
        assert_eq!(snap.proven[0].id.as_deref(), Some("router"));
        let rendered = render(&snap);
        assert!(rendered.contains("tanstack/router"));
        assert!(!rendered.contains("fastapi/fastapi"));
        assert!(!rendered.contains("Use Mapping for headers"));
    }

    #[tokio::test]
    async fn proven_rules_show_accepted_fix_evidence() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            "g1",
            "Return 413 for body size limit errors",
            Some("gin-gonic/gin"),
            "active",
            "2026-05-04T10:00:00Z",
        )
        .await;
        sqlx::query!(
            "INSERT INTO fix_outcomes \
             (id, rule_id, rule_name, file_path, accepted, applied_ok, created_at) \
             VALUES \
             ('f1', 'g1', 'Return 413 for body size limit errors', 'binding/binding.go', 1, 1, '2026-05-05T10:00:00Z'), \
             ('f2', 'g1', 'Return 413 for body size limit errors', 'binding/binding.go', 1, 1, '2026-05-05T11:00:00Z'), \
             ('f3', 'g1', 'Return 413 for body size limit errors', 'binding/binding.go', 0, 1, '2026-05-05T12:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let snap = load(&pool).await;
        assert_eq!(snap.proven.len(), 1);
        assert_eq!(snap.proven[0].id.as_deref(), Some("g1"));
        assert_eq!(snap.proven[0].accepted_count, 2);
        assert_eq!(snap.proven[0].accepted_fix_proofs, 2);
        assert_eq!(snap.proven[0].accepted_hook_outcomes, 0);
        assert_eq!(
            snap.proven[0].sample_file.as_deref(),
            Some("binding/binding.go")
        );
        let rendered = render(&snap);
        assert!(rendered.contains("proven"));
        assert!(rendered.contains("Return 413 for body size limit errors"));
        assert!(rendered.contains("2 accepted fixes"));
        assert!(
            rendered.contains(
                "difflore recall \"Return 413 for body size limit errors\" --file \"binding/binding.go\" --top-k 3"
            ),
            "rendered = {rendered}"
        );
        assert!(
            !rendered.contains("difflore rules explain"),
            "rendered = {rendered}"
        );
    }

    #[tokio::test]
    async fn proven_rules_include_agent_hook_accepted_outcomes() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            "agent-rule",
            "Prefer structured API parsing",
            Some("acme/widgets"),
            "active",
            "2026-05-04T10:00:00Z",
        )
        .await;
        let hook_summaries = vec![
            difflore_core::cloud::observations::AcceptedFixOutcomeRuleSummary {
                rule_id: "agent-rule".to_owned(),
                accepted_outcomes: 2,
                linked_to_prior_recall: 1,
                linked_to_rule_recall: 0,
                linked_to_mcp_rule_serve: 1,
                linked_to_edit_attribution: 0,
                sample_file: Some("src/parser.rs".to_owned()),
                latest_occurred_at_ms: 123,
            },
        ];

        let proven = fetch_proven_with_hook_summaries(
            &pool,
            Some(&["acme/widgets".to_owned()]),
            &hook_summaries,
        )
        .await;

        assert_eq!(proven.len(), 1);
        assert_eq!(proven[0].id.as_deref(), Some("agent-rule"));
        assert_eq!(proven[0].accepted_count, 2);
        assert_eq!(proven[0].accepted_fix_proofs, 0);
        assert_eq!(proven[0].accepted_hook_outcomes, 2);
        assert_eq!(proven[0].accepted_hook_outcomes_linked_to_prior_recall, 1);
        assert_eq!(proven[0].accepted_hook_outcomes_linked_to_rule_recall, 0);
        assert_eq!(proven[0].accepted_hook_outcomes_linked_to_mcp_rule_serve, 1);
        assert_eq!(
            proven[0].accepted_hook_outcomes_linked_to_edit_attribution,
            0
        );
        assert_eq!(proven[0].sample_file.as_deref(), Some("src/parser.rs"));

        let rendered = render(&MemorySnapshot {
            total_rules: 1,
            proven,
            ..MemorySnapshot::default()
        });
        assert!(rendered.contains("2 accepted outcomes"));
        assert!(rendered.contains("2 agent/hook outcomes"));
        assert!(rendered.contains("1 linked to prior memory recall (1 agent recall)"));
        assert!(!rendered.contains("difflore rules explain"));
    }

    #[test]
    fn render_shows_actual_agent_citation_proof_when_available() {
        let snap = MemorySnapshot {
            total_rules: 1,
            agent_citation: Some(AgentCitationProof {
                actual_citations: 1,
                rule_fires: 3,
                pending_uploads: 1,
                pending_upload_issue: Some(ObservationUploadIssue::MissingCloudScope),
            }),
            ..MemorySnapshot::default()
        };

        let rendered = render(&snap);

        assert!(rendered.contains("agent"));
        assert!(rendered.contains("1 actual citation"));
        assert!(rendered.contains("3 memory fires in 7d"));
        assert!(rendered.contains("1 pending upload"));
        assert!(rendered.contains("activity queued safely"));
        assert!(rendered.contains("refresh login once"));
        assert!(rendered.contains("difflore cloud login"));
    }

    #[tokio::test]
    async fn proven_rules_prefer_current_repo_aliases_before_global_top() {
        let pool = fresh_pool().await;
        insert(
            &pool,
            "router",
            "Pin Actions to commit SHAs",
            Some("tanstack/router"),
            "active",
            "2026-05-04T10:00:00Z",
        )
        .await;
        insert(
            &pool,
            "gin",
            "BindAll returns 413 for MaxBytesError",
            Some("gin-gonic/gin"),
            "active",
            "2026-05-04T10:00:00Z",
        )
        .await;
        sqlx::query!(
            "INSERT INTO fix_outcomes \
             (id, rule_id, rule_name, file_path, accepted, applied_ok, created_at) \
             VALUES \
             ('r1', 'router', 'Pin Actions to commit SHAs', '.github/workflows/ci.yml', 1, 1, '2026-05-05T10:00:00Z'), \
             ('r2', 'router', 'Pin Actions to commit SHAs', '.github/workflows/ci.yml', 1, 1, '2026-05-05T11:00:00Z'), \
             ('r3', 'router', 'Pin Actions to commit SHAs', '.github/workflows/ci.yml', 1, 1, '2026-05-05T12:00:00Z'), \
             ('g1', 'gin', 'BindAll returns 413 for MaxBytesError', 'binding/binding.go', 1, 1, '2026-05-05T13:00:00Z')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let scoped = fetch_proven(
            &pool,
            &[
                "difflore-fixtures/gin".to_owned(),
                "gin-gonic/gin".to_owned(),
            ],
        )
        .await;
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].id.as_deref(), Some("gin"));
        assert_eq!(scoped[0].source_repo.as_deref(), Some("gin-gonic/gin"));
        assert_eq!(scoped[0].accepted_count, 1);
        assert_eq!(scoped[0].accepted_fix_proofs, 1);
        assert_eq!(scoped[0].accepted_hook_outcomes, 0);
        assert_eq!(scoped[0].sample_file.as_deref(), Some("binding/binding.go"));

        let fallback = fetch_proven(&pool, &["unknown/repo".to_owned()]).await;
        assert_eq!(fallback[0].id.as_deref(), Some("router"));
        assert_eq!(fallback[0].source_repo.as_deref(), Some("tanstack/router"));
        assert_eq!(fallback[0].accepted_count, 3);
    }
}
