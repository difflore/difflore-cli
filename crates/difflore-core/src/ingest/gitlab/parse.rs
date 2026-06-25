//! Conversion from GitLab wire shapes into the provider-neutral review
//! store, where the GitLab and GitHub import paths converge: same
//! `review_items` / `review_comments` rows, same durability-signal metadata,
//! so the local-candidate gate and the upload path need zero provider
//! branches.
//!
//! ID scheme (collision-proof against the GitHub importer):
//! * item id / external review id — `gl-import:{host}/{namespace_path}#{iid}`
//! * external comment id — `gl:{note_id}`
//! * `repo_full_name` stores the bare namespace path; the host lives in
//!   per-item metadata under the `gitlabHost` key.

use sqlx::SqlitePool;

use crate::ingest::ImportProgress;
use crate::ingest::common::{CommentDurabilitySignal, comment_exists, comment_metadata_json};
use crate::review_store::{AddCommentInput, EnsureItemInput};

use super::ImportOptions;
use super::schema::{Discussion, MergeRequest, Note};

/// `gl-import:{host}/{namespace_path}#{iid}` — host included so the same
/// `group/project` path on gitlab.com and a self-managed mirror never
/// collide in the store.
pub(super) fn gitlab_item_id(host: &str, project_path: &str, iid: i64) -> String {
    format!("gl-import:{host}/{project_path}#{iid}")
}

/// `gl:{note_id}` — prefixed so a GitLab note id can never collide with a
/// GitHub comment `databaseId` in the shared dedupe lookup.
pub(super) fn gitlab_external_comment_id(note_id: i64) -> String {
    format!("gl:{note_id}")
}

/// Per-item metadata carrying the instance host. The upload path reads
/// `sourceRepoFullName` from item metadata and correctly finds none here
/// (GitLab v1 has no fork-import flow).
pub(super) fn item_metadata_json(host: &str) -> String {
    serde_json::json!({ "gitlabHost": host }).to_string()
}

/// Convert a `--since YYYY-MM-DD` date into the ISO8601 instant GitLab's
/// `updated_after` parameter expects (midnight UTC).
pub(super) fn updated_after_param(since: &str) -> String {
    format!("{since}T00:00:00Z")
}

/// Discussion-level resolution: GitLab tracks `resolved` per note, but the
/// adoption proxy the durability signal wants is "the maintainer settled the
/// thread" — at least one resolvable note, all of them resolved.
pub(super) fn discussion_resolved(notes: &[Note]) -> bool {
    let mut saw_resolvable = false;
    for note in notes {
        if note.resolvable {
            saw_resolvable = true;
            if !note.resolved {
                return false;
            }
        }
    }
    saw_resolvable
}

/// Whether any discussion carries a human-importable note (non-system,
/// non-empty body). MRs failing this are dropped before they count toward
/// progress, mirroring the GitHub importer's "no empty-PR spam" rule.
pub(super) fn has_importable_notes(discussions: &[Discussion]) -> bool {
    discussions
        .iter()
        .flat_map(|d| d.notes.iter())
        .any(is_importable_note)
}

fn is_importable_note(note: &Note) -> bool {
    !note.system && !note.body.trim().is_empty()
}

/// Note permalink: GitLab has no per-note URL field, but `{web_url}#note_{id}`
/// is the documented anchor format.
pub(super) fn note_comment_url(mr_web_url: Option<&str>, note_id: i64) -> Option<String> {
    let url = mr_web_url?;
    Some(format!("{url}#note_{note_id}"))
}

/// Representative file path for the review item: first inline note's
/// `new_path`, else the MR title (same fallback the GitHub importer uses so
/// the item stays queryable).
pub(super) fn representative_file_path(mr: &MergeRequest, discussions: &[Discussion]) -> String {
    discussions
        .iter()
        .flat_map(|d| d.notes.iter())
        .filter(|note| is_importable_note(note))
        .find_map(|note| {
            note.position
                .as_ref()
                .and_then(|p| p.new_path.as_deref())
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| mr.title.clone())
}

/// Durability signal for one note: discussion resolution is the adoption
/// proxy, later non-system replies in the same discussion feed contradiction
/// detection.
///
/// v1 deliberately skips per-note award emoji (GitLab's 👍/👎 equivalent):
/// awards live behind `GET .../notes/:id/award_emoji`, one extra request per
/// note (N+1 against instances that already rate-limit hard). The signal is
/// neutral-by-default, so `thumbs_up`/`thumbs_down` stay 0 and the candidate
/// gate simply sees "no reaction evidence" — resolution and replies still
/// carry the routing decision.
pub(super) fn note_signal(discussion: &Discussion, note_index: usize) -> CommentDurabilitySignal {
    CommentDurabilitySignal {
        resolved: discussion_resolved(&discussion.notes),
        later_replies: discussion
            .notes
            .iter()
            .skip(note_index + 1)
            .filter(|note| is_importable_note(note))
            .map(|note| note.body.clone())
            .collect(),
        ..CommentDurabilitySignal::default()
    }
}

/// Persist one MR with its discussions: `ensure_item` for the MR, then one
/// `add_comment` per importable note — the exact persistence calls the
/// GitHub path makes, so everything downstream is provider-neutral.
pub(super) async fn persist_merge_request(
    db: &SqlitePool,
    opts: &ImportOptions,
    mr: &MergeRequest,
    discussions: &[Discussion],
    progress: &mut ImportProgress,
) -> crate::Result<()> {
    let item_id = gitlab_item_id(&opts.host, &opts.project_path, mr.iid);
    let file_path = representative_file_path(mr, discussions);

    crate::review_store::ensure_item(
        db,
        EnsureItemInput {
            id: Some(item_id.clone()),
            session_id: None,
            project_id: opts.project_id.clone(),
            file_path,
            diff_content: String::new(),
            status: "imported".into(),
            source: "gitlab".into(),
            source_kind: "gitlab_import".into(),
            external_review_id: Some(item_id.clone()),
            repo_full_name: Some(opts.project_path.clone()),
            pr_number: i32::try_from(mr.iid).ok(),
            author: mr.author.as_ref().map(|a| a.username.clone()),
            synced_at: None,
            metadata: Some(item_metadata_json(&opts.host)),
            reviewed_at: None,
        },
    )
    .await?;

    for discussion in discussions {
        for (index, note) in discussion.notes.iter().enumerate() {
            if !is_importable_note(note) {
                continue;
            }
            let external_id = gitlab_external_comment_id(note.id);
            if comment_exists(db, &external_id).await? {
                progress.comments_skipped += 1;
                continue;
            }
            let signal = note_signal(discussion, index);
            let inline_path = note
                .position
                .as_ref()
                .and_then(|p| p.new_path.as_deref())
                .map(str::trim)
                .filter(|p| !p.is_empty());
            // Non-inline notes are MR-level discussion comments — mark them
            // the way the GitHub path marks PR discussion comments so the
            // provenance key stays meaningful downstream.
            let source_kind = if note.position.is_some() {
                None
            } else {
                Some("mr_comment")
            };
            crate::review_store::add_comment(
                db,
                AddCommentInput {
                    review_item_id: item_id.clone(),
                    external_comment_id: Some(external_id),
                    line_number: note.position.as_ref().and_then(|p| p.new_line),
                    content: note.body.clone(),
                    author: note.author.as_ref().map(|a| a.username.clone()),
                    comment_url: note_comment_url(mr.web_url.as_deref(), note.id),
                    thread_id: Some(discussion.id.clone()),
                    metadata: Some(comment_metadata_json(
                        inline_path,
                        None,
                        &opts.project_path,
                        source_kind,
                        &signal,
                    )),
                },
            )
            .await?;
            progress.comments_imported += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: i64, body: &str, system: bool, resolvable: bool, resolved: bool) -> Note {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "body": body,
            "system": system,
            "resolvable": resolvable,
            "resolved": resolved,
            "author": { "id": 1, "username": "reviewer" },
        }))
        .expect("note fixture deserializes")
    }

    fn inline_note(id: i64, body: &str, path: &str, line: i32, resolved: bool) -> Note {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "body": body,
            "system": false,
            "resolvable": true,
            "resolved": resolved,
            "author": { "id": 1, "username": "reviewer" },
            "position": { "new_path": path, "new_line": line },
        }))
        .expect("inline note fixture deserializes")
    }

    fn discussion(id: &str, notes: Vec<Note>) -> Discussion {
        Discussion {
            id: id.to_owned(),
            notes,
        }
    }

    fn merge_request(iid: i64, title: &str) -> MergeRequest {
        serde_json::from_value(serde_json::json!({
            "iid": iid,
            "title": title,
            "author": { "id": 2, "username": "alice" },
            "web_url": format!("https://gitlab.com/group/project/-/merge_requests/{iid}"),
        }))
        .expect("MR fixture deserializes")
    }

    #[test]
    fn id_scheme_is_host_scoped_and_gl_prefixed() {
        assert_eq!(
            gitlab_item_id("gitlab.com", "group/sub/project", 42),
            "gl-import:gitlab.com/group/sub/project#42"
        );
        assert_eq!(
            gitlab_item_id("gitlab.corp.example:8443", "group/project", 7),
            "gl-import:gitlab.corp.example:8443/group/project#7"
        );
        assert_eq!(gitlab_external_comment_id(9001), "gl:9001");
    }

    #[test]
    fn item_metadata_carries_the_instance_host() {
        let value: serde_json::Value =
            serde_json::from_str(&item_metadata_json("gitlab.corp.example")).expect("json");
        assert_eq!(value["gitlabHost"], "gitlab.corp.example");
        assert!(
            value.get("sourceRepoFullName").is_none(),
            "no fork flow in v1 — the upload path must not see a source repo"
        );
    }

    #[test]
    fn comment_metadata_omits_gitlab_source_repo() {
        let signal = CommentDurabilitySignal::default();
        let value: serde_json::Value = serde_json::from_str(&comment_metadata_json(
            Some("src/lib.rs"),
            None,
            "group/project",
            Some("mr_comment"),
            &signal,
        ))
        .expect("json");

        assert!(value.get("sourceRepoFullName").is_none());
        assert_eq!(value["attachedRepoFullName"], "group/project");
        assert_eq!(value["sourceKind"], "mr_comment");
    }

    #[test]
    fn updated_after_is_midnight_utc_of_the_since_date() {
        assert_eq!(updated_after_param("2026-01-15"), "2026-01-15T00:00:00Z");
    }

    #[test]
    fn discussion_resolution_requires_all_resolvable_notes_resolved() {
        // All resolvable notes resolved → resolved.
        assert!(discussion_resolved(&[
            note(1, "a", false, true, true),
            note(2, "b", false, true, true),
        ]));
        // One unresolved resolvable note → not resolved.
        assert!(!discussion_resolved(&[
            note(1, "a", false, true, true),
            note(2, "b", false, true, false),
        ]));
        // No resolvable notes at all (MR-level comment) → neutral false.
        assert!(!discussion_resolved(&[note(1, "a", false, false, false)]));
        assert!(!discussion_resolved(&[]));
    }

    #[test]
    fn note_signal_collects_later_replies_and_skips_system_noise() {
        let d = discussion(
            "abc",
            vec![
                inline_note(1, "Validate the header first.", "src/lib.rs", 3, true),
                note(2, "added 1 commit", true, false, false), // system
                note(3, "Done, thanks!", false, true, true),
            ],
        );
        let signal = note_signal(&d, 0);
        assert!(signal.resolved);
        assert_eq!(signal.later_replies, vec!["Done, thanks!".to_owned()]);
        // v1 emoji skip: reactions stay neutral.
        assert_eq!(signal.reactions_total, 0);
        assert_eq!(signal.thumbs_up, 0);
        assert_eq!(signal.thumbs_down, 0);

        // The last note has no later replies.
        let tail = note_signal(&d, 2);
        assert!(tail.later_replies.is_empty());
    }

    #[test]
    fn representative_path_prefers_inline_anchor_then_title() {
        let mr = merge_request(42, "Validate request headers");
        let inline = vec![
            discussion(
                "top",
                vec![note(1, "Please add a changelog.", false, false, false)],
            ),
            discussion(
                "inline",
                vec![inline_note(
                    2,
                    "Check this.",
                    "src/http/request.rs",
                    9,
                    false,
                )],
            ),
        ];
        assert_eq!(
            representative_file_path(&mr, &inline),
            "src/http/request.rs"
        );

        let no_inline = vec![discussion(
            "top",
            vec![note(1, "Please add a changelog.", false, false, false)],
        )];
        assert_eq!(
            representative_file_path(&mr, &no_inline),
            "Validate request headers"
        );
    }

    #[test]
    fn importable_note_gate_drops_system_and_empty_bodies() {
        assert!(has_importable_notes(&[discussion(
            "d",
            vec![note(1, "real feedback", false, false, false)]
        )]));
        assert!(!has_importable_notes(&[discussion(
            "d",
            vec![
                note(1, "changed the description", true, false, false),
                note(2, "   ", false, false, false),
            ]
        )]));
        assert!(!has_importable_notes(&[]));
    }

    #[test]
    fn note_urls_anchor_on_the_mr_web_url() {
        assert_eq!(
            note_comment_url(
                Some("https://gitlab.com/group/project/-/merge_requests/42"),
                9001
            )
            .as_deref(),
            Some("https://gitlab.com/group/project/-/merge_requests/42#note_9001")
        );
        assert_eq!(note_comment_url(None, 9001), None);
    }

    #[tokio::test]
    async fn persist_merge_request_round_trips_and_dedupes_on_rerun() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                <sqlx::sqlite::SqliteConnectOptions as std::str::FromStr>::from_str(
                    "sqlite::memory:",
                )
                .expect("parse sqlite memory URL")
                .foreign_keys(true),
            )
            .await
            .expect("open in-memory db");
        crate::infra::db::run_migrations(&pool)
            .await
            .expect("apply migrations");
        let dir = tempfile::TempDir::new().expect("tempdir");
        let project = crate::domain::projects::add(
            &pool,
            crate::domain::models::AddProjectInput {
                path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await
        .expect("insert project");

        let opts = ImportOptions {
            host: "gitlab.com".to_owned(),
            project_path: "group/sub/project".to_owned(),
            project_id: project.id,
            token: "unused-by-persist".to_owned(),
            max_mrs: 50,
            mr_iids: Vec::new(),
            exclude_mrs: std::collections::HashSet::new(),
            since: None,
        };
        let mr = merge_request(42, "Validate request headers");
        let discussions = vec![
            discussion(
                "thread-1",
                vec![
                    inline_note(
                        9001,
                        "Validate the header first.",
                        "src/http/request.rs",
                        12,
                        true,
                    ),
                    note(9002, "Done, thanks!", false, true, true),
                ],
            ),
            discussion(
                "sys",
                vec![note(9003, "changed the description", true, false, false)],
            ),
            discussion(
                "top",
                vec![note(
                    9004,
                    "Please add a changelog entry.",
                    false,
                    false,
                    false,
                )],
            ),
        ];

        let mut progress = ImportProgress {
            prs_fetched: 0,
            prs_total: 1,
            comments_imported: 0,
            comments_skipped: 0,
            prs_missing: 0,
            missing_pr_numbers: Vec::new(),
        };
        persist_merge_request(&pool, &opts, &mr, &discussions, &mut progress)
            .await
            .expect("persist");
        assert_eq!(progress.comments_imported, 3, "system note must be dropped");
        assert_eq!(progress.comments_skipped, 0);

        let items = crate::review_store::list_by_source_with_comments(
            &pool,
            crate::review_store::ReviewSourceInput {
                source: "gitlab".into(),
            },
        )
        .await
        .expect("list gitlab items");
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.item.id, "gl-import:gitlab.com/group/sub/project#42");
        assert_eq!(item.item.source_kind, "gitlab_import");
        assert_eq!(
            item.item.repo_full_name.as_deref(),
            Some("group/sub/project"),
            "repo_full_name stores the bare namespace path"
        );
        assert_eq!(item.item.pr_number, Some(42));
        assert_eq!(item.item.file_path, "src/http/request.rs");
        let item_meta: serde_json::Value =
            serde_json::from_str(item.item.metadata.as_deref().expect("item metadata"))
                .expect("metadata json");
        assert_eq!(item_meta["gitlabHost"], "gitlab.com");

        assert_eq!(item.comments.len(), 3);
        let inline = item
            .comments
            .iter()
            .find(|c| c.external_comment_id.as_deref() == Some("gl:9001"))
            .expect("inline note imported");
        assert_eq!(inline.line_number, Some(12));
        assert_eq!(inline.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(
            inline.comment_url.as_deref(),
            Some("https://gitlab.com/group/project/-/merge_requests/42#note_9001")
        );
        let inline_meta: serde_json::Value =
            serde_json::from_str(inline.metadata.as_deref().expect("comment metadata"))
                .expect("comment metadata json");
        assert_eq!(inline_meta["filePath"], "src/http/request.rs");
        assert_eq!(inline_meta["resolved"], true);
        assert_eq!(inline_meta["laterReplies"][0], "Done, thanks!");

        let top_level = item
            .comments
            .iter()
            .find(|c| c.external_comment_id.as_deref() == Some("gl:9004"))
            .expect("MR-level note imported");
        let top_meta: serde_json::Value =
            serde_json::from_str(top_level.metadata.as_deref().expect("metadata"))
                .expect("metadata json");
        assert_eq!(top_meta["sourceKind"], "mr_comment");
        assert!(top_meta["filePath"].is_null());

        // Re-run: everything dedupes via the gl:-prefixed external ids.
        let mut rerun = ImportProgress {
            prs_fetched: 0,
            prs_total: 1,
            comments_imported: 0,
            comments_skipped: 0,
            prs_missing: 0,
            missing_pr_numbers: Vec::new(),
        };
        persist_merge_request(&pool, &opts, &mr, &discussions, &mut rerun)
            .await
            .expect("rerun persists");
        assert_eq!(rerun.comments_imported, 0);
        assert_eq!(rerun.comments_skipped, 3);
    }
}
