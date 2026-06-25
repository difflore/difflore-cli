//! Shared building blocks for the "proven rule" drilldown that both
//! `status` (`queries::proven_rule`) and `doctor` (`memory_snapshot`) compute.
//!
//! The two surfaces differ in scope handling, output shape, and the exact
//! signed-proof SQL, but they share two pieces verbatim: the rule-metadata
//! lookup used to attach a name/source-repo to agent-hook outcomes, and the
//! candidate ranking order. Hoisting those here keeps the two from silently
//! disagreeing about which rule is "the" proven one.

use std::cmp::Ordering;
use std::collections::HashMap;

/// Rule name + source repo loaded by id, used to attach metadata to
/// agent-hook accepted outcomes (which carry only a `rule_id`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct RuleMetadataRow {
    pub(crate) rule_id: String,
    pub(crate) name: String,
    pub(crate) source_repo: Option<String>,
}

/// Load active-rule metadata for `rule_ids`, optionally constrained to
/// `normalized_repos`. Returns a map keyed by rule id; ids that are empty
/// (after trimming) are dropped, and an empty input yields an empty map.
pub(crate) async fn fetch_rule_metadata_for_ids(
    db: &difflore_core::SqlitePool,
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
        "SELECT id AS rule_id,
                COALESCE(NULLIF(name, ''), id) AS name,
                source_repo AS source_repo
         FROM skills
         WHERE id IN ({id_placeholders})
           AND COALESCE(status, 'active') = 'active'
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
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.rule_id.clone(), row))
        .collect()
}

/// Ranking key for a proven-rule candidate. Both surfaces order candidates by
/// total accepted (signed + hook) desc, then outcomes linked to prior recall
/// desc, then signed fix proofs desc, then name asc as a stable tiebreaker.
pub(crate) struct ProvenRuleRank<'a> {
    pub(crate) total: i64,
    pub(crate) linked_to_prior_recall: i64,
    pub(crate) accepted_fix_proofs: i64,
    pub(crate) name: &'a str,
}

impl ProvenRuleRank<'_> {
    pub(crate) fn cmp(&self, other: &Self) -> Ordering {
        other
            .total
            .cmp(&self.total)
            .then(
                other
                    .linked_to_prior_recall
                    .cmp(&self.linked_to_prior_recall),
            )
            .then(other.accepted_fix_proofs.cmp(&self.accepted_fix_proofs))
            .then(self.name.cmp(other.name))
    }
}
