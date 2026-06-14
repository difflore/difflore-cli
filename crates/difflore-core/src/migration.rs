//! Retired local persistence migrations — live startup guard.
//!
//! Installs create per-project context indexes at
//! `~/.difflore/projects/{project_hash}/context-index.db`. The former global
//! `~/.difflore/context-index.db` split migration is retired; runtimes must
//! not copy or reinterpret historical index contents.
//!
//! NOTE (R1/R4): the *migration* is retired, but this module is NOT dead code
//! and must not be deleted. [`run_if_needed`] is a live fail-closed guard
//! called from `difflore-cli/src/lib.rs` on every startup, and it is covered
//! by `tests/migration_test.rs`. Removing it would drop the protection against
//! silently reinterpreting a stale global index. See ARCHITECTURE.md.

use crate::context::index_db;
use crate::error::CoreError;

/// Startup guard for retired local index layouts.
///
/// If a retired global `context-index.db` is present, fail closed and leave
/// it untouched. Users can delete/move it and let the per-project index
/// rebuild from canonical rules.
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
