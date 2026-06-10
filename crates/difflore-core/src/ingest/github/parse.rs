//! Comment parsing, durability-signal derivation, metadata serialization,
//! and PR filtering/dedup helpers.
//!
//! Turns the raw GraphQL wire shapes in `schema.rs` into the comment metadata
//! and candidate set the importer persists. Holds no HTTP / `gh`-CLI concerns.

use super::schema::{PrNode, ReactionGroupNode};

/// Durability signal for a single comment, derived from the GraphQL
/// thread/reaction shape and serialized into the comment metadata JSON so the
/// local-candidate gate can read it back without a second GitHub round-trip.
/// Every field is neutral-by-default so a missing GraphQL field never blocks
/// import.
#[derive(Debug, Default, Clone)]
pub(super) struct CommentDurabilitySignal {
    /// The parent review thread was resolved (adoption proxy).
    pub(super) resolved: bool,
    pub(super) reactions_total: i64,
    pub(super) thumbs_up: i64,
    pub(super) thumbs_down: i64,
    /// Bodies of replies that came after this comment in the same thread, used
    /// to detect a later contradiction. Empty for review bodies and standalone
    /// issue comments.
    pub(super) later_replies: Vec<String>,
}

impl CommentDurabilitySignal {
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

    /// Serialize the non-neutral fields into the metadata object for the
    /// confidence gate. Returns `None` for an all-neutral signal so metadata
    /// stays byte-identical for comments with no signal.
    pub(super) fn to_metadata_value(&self) -> Option<serde_json::Value> {
        if !self.resolved
            && self.reactions_total == 0
            && self.thumbs_up == 0
            && self.thumbs_down == 0
            && self.later_replies.is_empty()
        {
            return None;
        }
        Some(serde_json::json!({
            "resolved": self.resolved,
            "reactionsTotal": self.reactions_total,
            "thumbsUp": self.thumbs_up,
            "thumbsDown": self.thumbs_down,
            "laterReplies": &self.later_replies,
        }))
    }
}

pub(super) fn imported_external_id(repo: &str, source_repo: &str, db_id: i64) -> String {
    if repo == source_repo {
        db_id.to_string()
    } else {
        format!("{repo}:{source_repo}:{db_id}")
    }
}

/// Build the per-comment metadata JSON string, merging the provenance keys
/// (`filePath` / `sourceRepoFullName` / `attachedRepoFullName` / `sourceKind`)
/// with the durability signal keys. Durability keys are added only when
/// non-neutral, so a comment with no signal serializes to the legacy shape.
pub(super) fn comment_metadata_json(
    file_path: Option<&str>,
    source_repo: &str,
    attached_repo: &str,
    source_kind: Option<&str>,
    signal: &CommentDurabilitySignal,
) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "filePath".to_owned(),
        file_path.map_or(serde_json::Value::Null, |p| {
            serde_json::Value::String(p.to_owned())
        }),
    );
    obj.insert(
        "sourceRepoFullName".to_owned(),
        serde_json::Value::String(source_repo.to_owned()),
    );
    obj.insert(
        "attachedRepoFullName".to_owned(),
        serde_json::Value::String(attached_repo.to_owned()),
    );
    if let Some(kind) = source_kind {
        obj.insert(
            "sourceKind".to_owned(),
            serde_json::Value::String(kind.to_owned()),
        );
    }
    if let Some(serde_json::Value::Object(signal_obj)) = signal.to_metadata_value() {
        obj.extend(signal_obj);
    }
    serde_json::Value::Object(obj).to_string()
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
    fn neutral_signal_serializes_to_none_so_legacy_metadata_is_unchanged() {
        let signal = CommentDurabilitySignal::default();
        assert!(signal.to_metadata_value().is_none());
        // The merged metadata for a no-signal comment must keep exactly the
        // legacy provenance keys (no durability keys leak in).
        let json = comment_metadata_json(Some("src/lib.rs"), "acme/up", "acme/fork", None, &signal);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["filePath"], "src/lib.rs");
        assert_eq!(value["sourceRepoFullName"], "acme/up");
        assert!(value.get("resolved").is_none());
        assert!(value.get("reactionsTotal").is_none());
        assert!(value.get("laterReplies").is_none());
    }

    #[test]
    fn resolved_thread_with_replies_round_trips_through_metadata() {
        let signal = CommentDurabilitySignal {
            resolved: true,
            reactions_total: 2,
            thumbs_up: 2,
            thumbs_down: 0,
            later_replies: vec!["Done, thanks!".to_owned()],
        };
        let json = comment_metadata_json(
            Some("src/lib.rs"),
            "acme/up",
            "acme/fork",
            Some("issue_comment"),
            &signal,
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["resolved"], true);
        assert_eq!(value["thumbsUp"], 2);
        assert_eq!(value["reactionsTotal"], 2);
        assert_eq!(value["sourceKind"], "issue_comment");
        assert_eq!(value["laterReplies"][0], "Done, thanks!");
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
