use std::path::PathBuf;

use difflore_core::SqlitePool;
use difflore_core::cloud::client::CloudClient;
use tokio::sync::OnceCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputMode {
    Json,
    Text,
}

impl OutputMode {
    pub const fn from_json_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Text }
    }
}

pub struct CommandContext {
    pub db: SqlitePool,
    pub project: PathBuf,
    pub mode: OutputMode,
    cloud_cell: OnceCell<CloudClient>,
}

impl CommandContext {
    pub async fn new(mode: OutputMode) -> Self {
        let db = crate::support::util::init_db().await;
        let project = difflore_core::infra::paths::current_project_root();
        Self {
            db,
            project,
            mode,
            cloud_cell: OnceCell::new(),
        }
    }

    /// Lazily construct the cloud client on first access. Construction always
    /// succeeds; a missing/unconfigured token surfaces as `client.token.is_none()`.
    pub async fn cloud(&self) -> &CloudClient {
        self.cloud_cell
            .get_or_init(|| async { CloudClient::create().await })
            .await
    }

    pub fn json(&self) -> bool {
        self.mode == OutputMode::Json
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::test_home::pin_test_home;

    #[tokio::test]
    async fn new_context_initialises_db_and_resolves_project_root() {
        pin_test_home();
        let ctx = CommandContext::new(OutputMode::Text).await;
        let row: (i64,) = sqlx::query_as("SELECT 1")
            .fetch_one(&ctx.db)
            .await
            .expect("trivial select must succeed");
        assert_eq!(row.0, 1);
        let expected = difflore_core::infra::paths::current_project_root();
        assert_eq!(ctx.project, expected);
        assert!(!ctx.json());
    }

    #[tokio::test]
    async fn json_mode_reports_json() {
        pin_test_home();
        let ctx = CommandContext::new(OutputMode::Json).await;
        assert!(ctx.json());
    }

    #[tokio::test]
    async fn cloud_is_lazy_and_memoised() {
        pin_test_home();
        let ctx = CommandContext::new(OutputMode::Text).await;
        let first = std::ptr::from_ref(ctx.cloud().await);
        let second = std::ptr::from_ref(ctx.cloud().await);
        assert_eq!(first, second, "second cloud() must hit the OnceCell");
    }

    #[test]
    fn output_mode_from_json_flag() {
        assert_eq!(OutputMode::from_json_flag(true), OutputMode::Json);
        assert_eq!(OutputMode::from_json_flag(false), OutputMode::Text);
    }
}
