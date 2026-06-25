//! Provider-neutral pieces of the review-import pipeline.
//!
//! The durability signal and the comment-metadata JSON shape are consumed by
//! the local-candidate gate and the cloud upload path, neither of which cares
//! which VCS provider produced the comment. Provider modules
//! ([`super::github`], [`super::gitlab`]) construct these from their own wire
//! shapes and persist the serialized form.

/// Check whether a comment with the given `external_comment_id` already
/// exists. Provider-neutral dedupe so re-running an import never duplicates
/// rows (GitHub uses raw database ids, GitLab `gl:`-prefixed note ids).
pub(crate) async fn comment_exists(
    db: &sqlx::SqlitePool,
    external_id: &str,
) -> crate::Result<bool> {
    let count = sqlx::query_scalar!(
        "SELECT COUNT(*) as \"n!: i64\" FROM review_comments WHERE external_comment_id = ?1",
        external_id
    )
    .fetch_one(db)
    .await?;
    Ok(count > 0)
}

/// Durability signal for a single review comment, derived from the provider's
/// thread/reaction shape and serialized into the comment metadata JSON so the
/// local-candidate gate can read it back without a second provider round-trip.
/// Every field is neutral-by-default so a missing provider field never blocks
/// import.
#[derive(Debug, Default, Clone)]
pub(crate) struct CommentDurabilitySignal {
    /// The parent review thread was resolved (adoption proxy).
    pub(crate) resolved: bool,
    pub(crate) reactions_total: i64,
    pub(crate) thumbs_up: i64,
    pub(crate) thumbs_down: i64,
    /// Bodies of replies that came after this comment in the same thread, used
    /// to detect a later contradiction. Empty for review bodies and standalone
    /// issue comments.
    pub(crate) later_replies: Vec<String>,
}

impl CommentDurabilitySignal {
    /// Serialize the non-neutral fields into the metadata object for the
    /// confidence gate. Returns `None` for an all-neutral signal so metadata
    /// stays byte-identical for comments with no signal.
    pub(crate) fn to_metadata_value(&self) -> Option<serde_json::Value> {
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

/// Build the per-comment metadata JSON string, merging the provenance keys
/// (`filePath` / `sourceRepoFullName` / `attachedRepoFullName` / `sourceKind`)
/// with the durability signal keys. Durability keys are added only when
/// non-neutral, so a comment with no signal serializes to the legacy shape.
pub(crate) fn comment_metadata_json(
    file_path: Option<&str>,
    source_repo: Option<&str>,
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
    if let Some(source_repo) = source_repo.map(str::trim).filter(|repo| !repo.is_empty()) {
        obj.insert(
            "sourceRepoFullName".to_owned(),
            serde_json::Value::String(source_repo.to_owned()),
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_signal_serializes_to_none_so_legacy_metadata_is_unchanged() {
        let signal = CommentDurabilitySignal::default();
        assert!(signal.to_metadata_value().is_none());
        // The merged metadata for a no-signal comment must keep exactly the
        // legacy provenance keys (no durability keys leak in).
        let json = comment_metadata_json(
            Some("src/lib.rs"),
            Some("acme/up"),
            "acme/fork",
            None,
            &signal,
        );
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
            Some("acme/up"),
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
}
