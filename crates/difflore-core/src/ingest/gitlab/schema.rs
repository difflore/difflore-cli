//! GitLab REST v4 response shapes (wire format only).
//!
//! Two endpoints feed the importer:
//! * `GET /projects/:id/merge_requests` → [`MergeRequest`] list
//! * `GET /projects/:id/merge_requests/:iid/discussions` → [`Discussion`]
//!   list, each carrying ordered [`Note`]s.
//!
//! These structs are pure deserialization glue; the logic that consumes them
//! lives in `parse.rs` and `mod.rs`. Every non-identity field defaults so an
//! older/self-managed GitLab that omits a key degrades to neutral instead of
//! failing the whole import.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(super) struct MergeRequest {
    /// Project-scoped MR number (what `!42` refers to). This is the identity
    /// the importer keys on — never the global `id`.
    pub(super) iid: i64,
    #[serde(default)]
    pub(super) title: String,
    pub(super) author: Option<UserRef>,
    /// Browser URL of the MR; note permalinks are derived as
    /// `{web_url}#note_{id}`.
    pub(super) web_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UserRef {
    pub(super) username: String,
}

/// One discussion (thread) on an MR. Single top-level comments arrive as a
/// discussion with `individual_note: true` and exactly one note.
#[derive(Debug, Deserialize)]
pub(super) struct Discussion {
    pub(super) id: String,
    #[serde(default)]
    pub(super) notes: Vec<Note>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Note {
    pub(super) id: i64,
    #[serde(default)]
    pub(super) body: String,
    pub(super) author: Option<UserRef>,
    /// `true` for GitLab-generated activity ("changed the description",
    /// "added 1 commit", …) — filtered out before import.
    #[serde(default)]
    pub(super) system: bool,
    /// Only diff notes are resolvable; `resolved` is meaningless when this
    /// is `false`.
    #[serde(default)]
    pub(super) resolvable: bool,
    #[serde(default)]
    pub(super) resolved: bool,
    /// Present only for inline (diff-anchored) notes.
    pub(super) position: Option<Position>,
}

/// Diff anchor of an inline note. `new_path`/`new_line` map to the imported
/// comment's file path and line; `new_line` is absent when the note sits on
/// a deleted line (old side only).
#[derive(Debug, Deserialize)]
pub(super) struct Position {
    pub(super) new_path: Option<String>,
    pub(super) new_line: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real-shape fixture: one merged MR as returned by
    /// `GET /projects/:id/merge_requests?state=merged`.
    const MR_FIXTURE: &str = r#"
    [
      {
        "id": 1234567,
        "iid": 42,
        "project_id": 278964,
        "title": "Validate request headers before parsing",
        "state": "merged",
        "author": { "id": 99, "username": "alice", "name": "Alice" },
        "web_url": "https://gitlab.com/group/sub/project/-/merge_requests/42",
        "updated_at": "2026-05-01T10:00:00.000Z"
      }
    ]"#;

    /// Trimmed real-shape fixture: discussions for an MR — one resolved
    /// inline thread with a reply, one system note, one MR-level comment.
    const DISCUSSIONS_FIXTURE: &str = r#"
    [
      {
        "id": "abc123",
        "individual_note": false,
        "notes": [
          {
            "id": 9001,
            "type": "DiffNote",
            "body": "We should validate the header before parsing.",
            "author": { "id": 7, "username": "reviewer" },
            "system": false,
            "resolvable": true,
            "resolved": true,
            "position": {
              "base_sha": "aaa",
              "head_sha": "bbb",
              "old_path": "src/http/request.rs",
              "new_path": "src/http/request.rs",
              "old_line": null,
              "new_line": 12
            }
          },
          {
            "id": 9002,
            "type": "DiffNote",
            "body": "Done, thanks!",
            "author": { "id": 8, "username": "author" },
            "system": false,
            "resolvable": true,
            "resolved": true,
            "position": null
          }
        ]
      },
      {
        "id": "sys456",
        "individual_note": true,
        "notes": [
          {
            "id": 9003,
            "body": "changed the description",
            "system": true
          }
        ]
      },
      {
        "id": "top789",
        "individual_note": true,
        "notes": [
          {
            "id": 9004,
            "body": "Please add a changelog entry for this.",
            "author": { "id": 7, "username": "reviewer" },
            "system": false,
            "resolvable": false,
            "resolved": false
          }
        ]
      }
    ]"#;

    #[test]
    fn merge_request_fixture_deserializes_iid_author_and_web_url() {
        let mrs: Vec<MergeRequest> = serde_json::from_str(MR_FIXTURE).expect("MR list parses");
        assert_eq!(mrs.len(), 1);
        let mr = &mrs[0];
        assert_eq!(mr.iid, 42);
        assert_eq!(mr.title, "Validate request headers before parsing");
        assert_eq!(
            mr.author.as_ref().map(|a| a.username.as_str()),
            Some("alice")
        );
        assert_eq!(
            mr.web_url.as_deref(),
            Some("https://gitlab.com/group/sub/project/-/merge_requests/42")
        );
    }

    #[test]
    fn discussions_fixture_separates_inline_system_and_top_level_notes() {
        let discussions: Vec<Discussion> =
            serde_json::from_str(DISCUSSIONS_FIXTURE).expect("discussion list parses");
        assert_eq!(discussions.len(), 3);

        let inline = &discussions[0];
        assert_eq!(inline.id, "abc123");
        assert_eq!(inline.notes.len(), 2);
        let first = &inline.notes[0];
        assert!(!first.system);
        assert!(first.resolvable && first.resolved);
        let position = first.position.as_ref().expect("inline note has position");
        assert_eq!(position.new_path.as_deref(), Some("src/http/request.rs"));
        assert_eq!(position.new_line, Some(12));
        // The reply lost its position (null) — must still deserialize.
        assert!(inline.notes[1].position.is_none());

        let system = &discussions[1].notes[0];
        assert!(system.system, "system note flag must survive the wire");

        let top_level = &discussions[2].notes[0];
        assert!(!top_level.system);
        assert!(!top_level.resolvable);
        assert!(top_level.position.is_none());
    }

    #[test]
    fn minimal_note_shape_defaults_every_optional_field() {
        // Self-managed instances on older GitLab versions omit fields freely;
        // a bare id+body note must parse with neutral defaults.
        let note: Note = serde_json::from_str(r#"{ "id": 1, "body": "x" }"#).expect("parses");
        assert!(!note.system);
        assert!(!note.resolvable);
        assert!(!note.resolved);
        assert!(note.position.is_none());
        assert!(note.author.is_none());
    }
}
