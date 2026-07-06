//! Provider-neutral pieces of the review-import pipeline.
//!
//! The durability signal and the comment-metadata JSON shape are consumed by
//! the local-candidate gate and the explicit sync/export path, neither of which cares
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
    /// Authors of the comments that came BEFORE this one in the same thread.
    /// Thread-context provenance for the candidate gate's trust classifier: a
    /// human comment replying in a thread a bot participated in earlier is
    /// classified as `human_override_bot`. Empty for thread starters, review
    /// bodies, and standalone comments.
    pub(crate) earlier_thread_authors: Vec<String>,
    /// Source-kind labels for those earlier comments, preserving provider actor
    /// type even when the login does not look bot-shaped.
    pub(crate) earlier_thread_source_kinds: Vec<String>,
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
            && self.earlier_thread_authors.is_empty()
            && self.earlier_thread_source_kinds.is_empty()
        {
            return None;
        }
        let mut value = serde_json::json!({
            "resolved": self.resolved,
            "reactionsTotal": self.reactions_total,
            "thumbsUp": self.thumbs_up,
            "thumbsDown": self.thumbs_down,
            "laterReplies": &self.later_replies,
        });
        // Emitted only when present so pre-linkage metadata keeps its legacy
        // byte shape.
        if !self.earlier_thread_authors.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert(
                "earlierThreadAuthors".to_owned(),
                serde_json::json!(&self.earlier_thread_authors),
            );
        }
        if !self.earlier_thread_source_kinds.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.insert(
                "earlierThreadSourceKinds".to_owned(),
                serde_json::json!(&self.earlier_thread_source_kinds),
            );
        }
        Some(value)
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
            earlier_thread_authors: vec!["coderabbitai[bot]".to_owned()],
            earlier_thread_source_kinds: vec!["bot:coderabbitai[bot]".to_owned()],
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
        assert_eq!(value["earlierThreadAuthors"][0], "coderabbitai[bot]");
        assert_eq!(
            value["earlierThreadSourceKinds"][0],
            "bot:coderabbitai[bot]"
        );
    }
}
