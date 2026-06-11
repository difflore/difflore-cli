//! Comment parsing, durability-signal derivation, and PR filtering/dedup
//! helpers.
//!
//! Turns the raw GraphQL wire shapes in `schema.rs` into the comment metadata
//! and candidate set the importer persists. Holds no HTTP / `gh`-CLI concerns.
//! The provider-neutral signal/metadata shapes live in
//! [`crate::ingest::common`]; this module only owns the GitHub-specific
//! constructor from GraphQL reaction groups.

use super::schema::{PrNode, ReactionGroupNode};
use crate::ingest::common::CommentDurabilitySignal;

impl CommentDurabilitySignal {
    /// Derive the reaction half of the signal from GitHub's GraphQL
    /// `reactionGroups` shape. Thread resolution and later replies are filled
    /// in by the caller, which sees the whole thread.
    pub(super) fn from_reaction_groups(groups: &[ReactionGroupNode]) -> Self {
        let mut signal = Self::default();
        for group in groups {
            let count = group.users.total_count.max(0);
            signal.reactions_total += count;
            match group.content.as_deref() {
                Some("THUMBS_UP") => signal.thumbs_up += count,
                Some("THUMBS_DOWN") => signal.thumbs_down += count,
                _ => {}
            }
        }
        signal
    }
}

pub(super) fn imported_external_id(repo: &str, source_repo: &str, db_id: i64) -> String {
    if repo == source_repo {
        db_id.to_string()
    } else {
        format!("{repo}:{source_repo}:{db_id}")
    }
}

/// Drop any fetched PR whose `number` is in `exclude_prs`, in place. Runs
/// before comments become candidates so an excluded PR contributes zero rules
/// — the leak-free guarantee `--exclude-prs` relies on for recall evaluation.
pub(super) fn drop_excluded_prs(
    collected: &mut Vec<PrNode>,
    exclude_prs: &std::collections::HashSet<i32>,
) {
    if exclude_prs.is_empty() {
        return;
    }
    collected.retain(|pr| pr.number.is_none_or(|n| !exclude_prs.contains(&n)));
}

#[cfg(test)]
mod tests {
    use super::super::schema::{ReactionGroupNode, ReactionUsersNode, ReviewThreadNode};
    use super::*;

    #[test]
    fn reaction_groups_roll_up_into_thumbs_and_total() {
        let groups = vec![
            ReactionGroupNode {
                content: Some("THUMBS_UP".to_owned()),
                users: ReactionUsersNode { total_count: 3 },
            },
            ReactionGroupNode {
                content: Some("THUMBS_DOWN".to_owned()),
                users: ReactionUsersNode { total_count: 1 },
            },
            ReactionGroupNode {
                content: Some("HEART".to_owned()),
                users: ReactionUsersNode { total_count: 2 },
            },
        ];
        let signal = CommentDurabilitySignal::from_reaction_groups(&groups);
        assert_eq!(signal.thumbs_up, 3);
        assert_eq!(signal.thumbs_down, 1);
        assert_eq!(signal.reactions_total, 6);
    }

    #[test]
    fn older_api_shape_without_reaction_or_resolved_fields_degrades_gracefully() {
        // A review-thread node missing both `isResolved` and `reactionGroups`
        // (an older GitHub GraphQL shape) must still deserialize, defaulting
        // to the neutral signal rather than erroring.
        let json = r#"{ "comments": { "nodes": [ { "databaseId": 1, "body": "x" } ] } }"#;
        let thread: ReviewThreadNode = serde_json::from_str(json).unwrap();
        assert!(!thread.is_resolved);
        let comment = &thread.comments.nodes[0];
        assert!(comment.reaction_groups.is_empty());
        let signal = CommentDurabilitySignal::from_reaction_groups(&comment.reaction_groups);
        assert_eq!(signal.reactions_total, 0);
    }

    #[test]
    fn drop_excluded_prs_removes_excluded_numbers_so_they_contribute_zero_rules() {
        // Build PrNodes through serde so the many `#[serde(default)]` fields
        // fill in; only `number` matters for the exclude filter.
        let pr = |number: i32| -> PrNode {
            serde_json::from_str(&format!(
                r#"{{ "number": {number}, "title": "pr {number}" }}"#
            ))
            .expect("PrNode deserializes from a number+title")
        };
        let mut collected = vec![pr(10), pr(20), pr(30)];

        let exclude: std::collections::HashSet<i32> = std::iter::once(20).collect();
        drop_excluded_prs(&mut collected, &exclude);

        let remaining: Vec<i32> = collected.iter().filter_map(|p| p.number).collect();
        assert_eq!(
            remaining,
            vec![10, 30],
            "excluded PR #20 must be dropped before its comments become candidates"
        );
        assert!(
            !remaining.contains(&20),
            "an excluded PR number must yield zero rules"
        );
    }

    #[test]
    fn drop_excluded_prs_is_a_noop_when_exclude_set_is_empty() {
        let pr = |number: i32| -> PrNode {
            serde_json::from_str(&format!(r#"{{ "number": {number}, "title": "x" }}"#))
                .expect("PrNode deserializes")
        };
        let mut collected = vec![pr(1), pr(2)];
        drop_excluded_prs(&mut collected, &std::collections::HashSet::new());
        let remaining: Vec<i32> = collected.iter().filter_map(|p| p.number).collect();
        assert_eq!(
            remaining,
            vec![1, 2],
            "empty exclude set must keep every PR"
        );
    }
}
