//! Retired local persistence migrations.
//!
//! Current installs create per-project context indexes directly at
//! `~/.difflore/projects/{project_hash}/context-index.db`. The former
//! global `~/.difflore/context-index.db` split migration is intentionally
//! retired: new runtimes must not copy or reinterpret historical index
//! contents.

use crate::context::index_db;
use crate::errors::CoreError;

/// Startup guard for retired local index layouts.
///
/// If a retired global `context-index.db` is present, fail closed and leave
/// it untouched. Users can delete/move the file and let the current
/// per-project index rebuild from canonical rules.
pub async fn run_if_needed() -> Result<(), CoreError> {
    let retired_global_index = index_db::retired_global_index_db_path()?;
    if retired_global_index.exists() {
        return Err(CoreError::Internal(format!(
            "retired context-index split migration refused retired global index at {}; \
             move or delete that file, then let per-project indexes rebuild",
            retired_global_index.display()
        )));
    }

    Ok(())
}
