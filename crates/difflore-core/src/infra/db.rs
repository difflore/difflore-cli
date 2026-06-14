// SAFETY scope: a single `std::env::set_var` inside a `OnceLock` for the
// shared test-home tempdir. Gated to run exactly once per process.
#![allow(unsafe_code)]

use sha1::{Digest, Sha1};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::path::{Path, PathBuf};

/// Crate-wide singleton `TempDir` giving every test module the same
/// `DIFFLORE_HOME`. Sets the env var exactly once and never clears it, so
/// a concurrent reader can't fall back to the user's real `~/.difflore`.
#[cfg(test)]
pub(crate) fn shared_test_home() -> &'static Path {
    use std::sync::OnceLock;
    use tempfile::TempDir;
    static HOME: OnceLock<TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = TempDir::new().expect("create shared test home tempdir");
        // SAFETY: OnceLock gates this closure to run exactly once per
        // test process. We intentionally NEVER `remove_var` — that
        // removal is what used to race against concurrent readers.
        unsafe {
            std::env::set_var("DIFFLORE_HOME", dir.path());
        }
        dir
    })
    .path()
}

#[cfg_attr(test, allow(clippy::unnecessary_wraps))]
pub(crate) fn difflore_dir() -> crate::Result<PathBuf> {
    // `DIFFLORE_HOME` lets integration tests redirect the data dir to a
    // tempdir without modifying $HOME / $USERPROFILE. Honoured first; falls
    // back to the standard ~/.difflore in production.
    if let Some(custom) = crate::infra::env::difflore_home() {
        return Ok(PathBuf::from(custom));
    }
    // In test binaries, route to the crate-wide `shared_test_home()` rather
    // than ~/.difflore: a missing `DIFFLORE_HOME` may mean a sibling test
    // removed it mid-flight, and we must never race `migrate!` against the
    // developer's real data.
    #[cfg(test)]
    {
        Ok(shared_test_home().to_path_buf())
    }
    #[cfg(not(test))]
    Ok(dirs::home_dir()
        .ok_or_else(|| "cannot resolve home directory".to_owned())?
        .join(".difflore"))
}

/// Path to the global data.db. Stays global because cross-project features
/// (e.g. `rules stats`) rely on a single aggregate view; only the
/// per-project embedding index lives outside the global root.
pub fn data_db_path() -> crate::Result<PathBuf> {
    Ok(difflore_dir()?.join("data.db"))
}

/// Stable 12-hex-char (48-bit) SHA-1 of a lexical, slash-normalised path
/// identity. Deliberately avoids `canonicalize()` so project identity does
/// not shift when a directory is created/deleted or when Windows returns an
/// extended `\\?\` path. Suitable for local DB partition names, not as a
/// security boundary (48-bit collision space).
pub fn project_hash_from_root(root: &Path) -> String {
    let as_str = stable_project_identity(root);
    let mut hasher = Sha1::new();
    hasher.update(as_str.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn stable_project_identity(root: &Path) -> String {
    let raw = root.to_string_lossy().replace('\\', "/");
    let raw = strip_windows_extended_prefix(raw.trim());
    let absolute = if is_absolute_like(&raw) {
        raw
    } else if let Ok(cwd) = std::env::current_dir() {
        format!("{}/{}", cwd.to_string_lossy().replace('\\', "/"), raw)
    } else {
        raw
    };
    lexical_normalize_path(&absolute)
}

fn strip_windows_extended_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = path.strip_prefix("//?/") {
        rest.to_owned()
    } else if let Some(rest) = path.strip_prefix("//./") {
        rest.to_owned()
    } else {
        path.to_owned()
    }
}

fn is_absolute_like(path: &str) -> bool {
    path.starts_with('/')
        || (path.len() >= 3
            && path.as_bytes()[1] == b':'
            && path.as_bytes()[2] == b'/'
            && path.as_bytes()[0].is_ascii_alphabetic())
}

fn lexical_normalize_path(path: &str) -> String {
    let path = strip_windows_extended_prefix(path).replace('\\', "/");
    let (prefix, rest, absolute) = if path.len() >= 3
        && path.as_bytes()[1] == b':'
        && path.as_bytes()[2] == b'/'
        && path.as_bytes()[0].is_ascii_alphabetic()
    {
        (
            format!("{}:/", char::from(path.as_bytes()[0]).to_ascii_lowercase()),
            &path[3..],
            true,
        )
    } else if path.starts_with("//") {
        ("//".to_owned(), path.trim_start_matches('/'), true)
    } else if let Some(rest) = path.strip_prefix('/') {
        ("/".to_owned(), rest, true)
    } else {
        (String::new(), path.as_str(), false)
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }

    if parts.is_empty() {
        match prefix.as_str() {
            "" => ".".to_owned(),
            "/" => "/".to_owned(),
            "//" => "//".to_owned(),
            _ if prefix.ends_with(":/") => prefix.trim_end_matches('/').to_owned(),
            _ => prefix,
        }
    } else if prefix == "/" || prefix == "//" || prefix.ends_with(":/") {
        format!("{prefix}{}", parts.join("/"))
    } else {
        parts.join("/")
    }
}

/// Current project root via `git rev-parse --show-toplevel`, falling back to
/// the cwd (or `.` if even `current_dir` errors). Never panics.
pub fn current_project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output();
    if let Ok(out) = output
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !s.is_empty() {
            return PathBuf::from(s);
        }
    }
    cwd
}

/// Base dir for per-project index DBs: `~/.difflore/projects/{hash}/`.
/// Does not create the directory; callers needing it on disk must
/// `create_dir_all`.
pub fn project_index_dir(project_hash: &str) -> PathBuf {
    let mut p = difflore_dir().unwrap_or_else(|_| PathBuf::from(".difflore"));
    p.push("projects");
    p.push(project_hash);
    p
}

/// Process-wide async lock around `sqlx::migrate!`. Without it, two tokio
/// runtimes concurrently running the migrate runner (common in tests) both
/// contend on sqlx's `_sqlx_migrations` row lock and intermittently fail.
/// Covers only the migration step; `SqlitePool` is `Clone` via internal
/// `Arc`, so the cost is a few ms per unique call site.
static MIGRATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run every pending migration from `./migrations` against the given pool.
/// Centralised so `sqlx::migrate!` expands once per crate and every path is
/// guarded by `MIGRATION_LOCK`.
pub async fn run_migrations(pool: &SqlitePool) -> crate::Result<()> {
    let _guard = MIGRATION_LOCK.lock().await;
    // `sqlx::migrate!` embeds the migration files at compile time so
    // `cargo install difflore-cli` doesn't need the registry source to
    // persist post-install (the disk-reading `Migrator::new(path)` form
    // broke after `cargo cache clean`).
    Ok(sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| format!("migration failed: {e}"))?)
}

/// Cache of opened `data.db` pools keyed by resolved path, so all callers
/// share one pool per DB file instead of racing on migrations. Must be a
/// `tokio::sync::Mutex` (not `std`): the open+migrate pipeline is
/// `await`-heavy and the critical section is held across those awaits so a
/// second caller sees the finished pool, not a half-initialised DB.
static POOL_CACHE: tokio::sync::Mutex<Option<std::collections::HashMap<PathBuf, SqlitePool>>> =
    tokio::sync::Mutex::const_new(None);

/// Best-effort restrict a `~/.difflore` path to the owner on Unix (dir →
/// 0700, file → 0600), keeping the encrypted token and review data off other
/// users on a shared host. No-op on Windows (profile dir is already
/// ACL-restricted). Failures are ignored — this is hardening, not correctness.
#[cfg(unix)]
pub(crate) fn restrict_to_owner(path: &Path, is_dir: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if is_dir { 0o700 } else { 0o600 };
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
pub(crate) const fn restrict_to_owner(_path: &Path, _is_dir: bool) {}

/// Restrict a SQLite DB file and its WAL/SHM/journal sidecars to 0600 (Unix).
/// Missing sidecars are silently skipped.
#[cfg(unix)]
pub(crate) fn restrict_sqlite_files(db_path: &Path) {
    restrict_to_owner(db_path, false);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = db_path.as_os_str().to_owned();
        sidecar.push(suffix);
        restrict_to_owner(Path::new(&sidecar), false);
    }
}

#[cfg(not(unix))]
pub(crate) const fn restrict_sqlite_files(_db_path: &Path) {}

pub async fn init_db() -> crate::Result<SqlitePool> {
    let dir = difflore_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| crate::CoreError::Internal(format!("failed to create ~/.difflore: {e}")))?;
    restrict_to_owner(&dir, true);
    let db_path = dir.join("data.db");

    // Hold the cache lock across the whole open+migrate flow so only one
    // caller per DB path runs migrations; concurrent callers wait and get the
    // finished pool on the second pass.
    let mut guard = POOL_CACHE.lock().await;
    let cache = guard.get_or_insert_with(std::collections::HashMap::new);

    if let Some(pool) = cache.get(&db_path) {
        return Ok(pool.clone());
    }

    let opts = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .map_err(|e| {
            // A WAL database can't open on a read-only home (it must write the
            // `-shm`/`-wal` sidecars), surfacing as SQLITE_CANTOPEN — usually a
            // sandboxed MCP client confining writes to the workspace. Turn the
            // cryptic code into an actionable hint; read-only open isn't
            // supported (an existing `-wal` defeats `immutable=1`).
            if is_readonly_home_open_error(&e) {
                crate::CoreError::Internal(format!(
                    "failed to open data.db: {e}\n\
                     hint: ~/.difflore appears read-only. A sandboxed agent (e.g. codex with a \
                     restrictive --sandbox) blocks DiffLore's writes. Run DiffLore unsandboxed for \
                     that client, or set DIFFLORE_HOME to a writable path."
                ))
            } else {
                crate::CoreError::Internal(format!("failed to open data.db: {e}"))
            }
        })?;

    restrict_sqlite_files(&db_path);

    run_migrations(&pool).await?;

    cache.insert(db_path.clone(), pool.clone());
    Ok(pool)
}

/// True when a SQLite open failure indicates a read-only data home (a WAL
/// database can't write its `-shm`/`-wal` sidecars), surfacing as
/// `SQLITE_CANTOPEN` / `SQLITE_READONLY`.
fn is_readonly_home_open_error(err: &sqlx::Error) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("code: 14") // SQLITE_CANTOPEN
        || s.contains("code: 8") // SQLITE_READONLY
        || s.contains("unable to open database file")
        || s.contains("readonly")
        || s.contains("read-only")
}

/// Count rows in the named tables for the `difflore doctor` inventory.
/// Missing tables surface as `Err(message)` rather than aborting, so the
/// report stays best-effort.
pub async fn table_counts(pool: &SqlitePool, tables: &[&str]) -> Vec<(String, crate::Result<i64>)> {
    let mut out = Vec::with_capacity(tables.len());
    for t in tables {
        let count: crate::Result<i64> = match *t {
            "skills" => sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM skills"#)
                .fetch_one(pool)
                .await
                .map_err(crate::CoreError::Database),
            "review_items" => {
                sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM review_items"#)
                    .fetch_one(pool)
                    .await
                    .map_err(crate::CoreError::Database)
            }
            "review_comments" => {
                sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM review_comments"#)
                    .fetch_one(pool)
                    .await
                    .map_err(crate::CoreError::Database)
            }
            "providers" => sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM providers"#)
                .fetch_one(pool)
                .await
                .map_err(crate::CoreError::Database),
            "cloud_outbox" => {
                sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM cloud_outbox"#)
                    .fetch_one(pool)
                    .await
                    .map_err(crate::CoreError::Database)
            }
            "projects" => sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM projects"#)
                .fetch_one(pool)
                .await
                .map_err(crate::CoreError::Database),
            other => Err(format!("unknown table: {other}").into()),
        };
        out.push((t.to_string(), count));
    }
    out
}

/// Aggregate snapshot of the local skills (rules) corpus for
/// `difflore doctor --report`: total count, breakdown by `origin` and top
/// `source_repo`, and the count of skills with empty `file_patterns`
/// (recall-killing signature; the counter catches cluster-pipeline
/// regressions).
#[derive(Debug, Default)]
pub struct CorpusHealth {
    pub total: i64,
    pub by_origin: Vec<(String, i64)>,
    pub by_source_repo: Vec<(String, i64)>,
    pub empty_file_patterns: i64,
}

pub async fn corpus_health(pool: &SqlitePool) -> crate::Result<CorpusHealth> {
    let total =
        sqlx::query_scalar!("SELECT COUNT(*) as \"n!: i64\" FROM skills WHERE status = 'active'")
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;

    let by_origin_rows = sqlx::query!(
        "SELECT COALESCE(origin, '<unknown>') AS \"origin!: String\", COUNT(*) AS \"n!: i64\" FROM skills \
         WHERE status = 'active' GROUP BY origin ORDER BY COUNT(*) DESC"
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    let by_origin: Vec<(String, i64)> = by_origin_rows
        .into_iter()
        .map(|r| (r.origin, r.n))
        .collect();

    // `source_repo` is the single provenance column for rule attribution.
    let by_source_repo_rows = sqlx::query_as::<_, (Option<String>, i64)>(
        "WITH skill_repos AS ( \
             SELECT source_repo AS repo \
             FROM skills WHERE status = 'active' \
         ) \
         SELECT repo, COUNT(*) AS n \
         FROM skill_repos \
         GROUP BY repo \
         ORDER BY n DESC, COALESCE(repo, '') ASC \
         LIMIT 10",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    let by_source_repo: Vec<(String, i64)> = by_source_repo_rows
        .into_iter()
        .map(|(repo, n)| (repo.unwrap_or_else(|| "<unset>".to_owned()), n))
        .collect();

    let empty = sqlx::query_scalar!(
        "SELECT COUNT(*) as \"n!: i64\" FROM skills WHERE status = 'active' \
         AND (file_patterns IS NULL OR file_patterns = '' OR file_patterns = '[]')"
    )
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(CorpusHealth {
        total,
        by_origin,
        by_source_repo,
        empty_file_patterns: empty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_hash_is_stable_across_calls() {
        let p = PathBuf::from("/some/path/to/project");
        let h1 = project_hash_from_root(&p);
        let h2 = project_hash_from_root(&p);
        assert_eq!(h1, h2, "same path must hash to same value");
        assert_eq!(h1.len(), 12, "hash length must be 12 hex chars");
        assert!(
            h1.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be hex only: {h1}"
        );
    }

    #[test]
    fn restrict_to_owner_tightens_perms_without_erroring() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("secret_dir");
        std::fs::create_dir(&dir).unwrap();
        let file = tmp.path().join("data.db");
        std::fs::write(&file, b"token").unwrap();

        // Must not panic/error on any platform (a no-op on Windows). Sidecars
        // are absent here, so restrict_sqlite_files must silently skip them.
        restrict_to_owner(&dir, true);
        restrict_to_owner(&file, false);
        restrict_sqlite_files(&file);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700, "~/.difflore should be 0700");
            assert_eq!(mode(&file), 0o600, "the token/data DB should be 0600");
        }
    }

    #[test]
    fn project_hash_differs_for_different_roots() {
        let a = project_hash_from_root(&PathBuf::from("/projects/alpha"));
        let b = project_hash_from_root(&PathBuf::from("/projects/beta"));
        assert_ne!(a, b, "different roots should hash differently");
    }

    #[test]
    fn project_hash_normalises_windows_separator_variants() {
        // `\` is normalised to `/` before hashing, so the same logical path
        // collapses to the same hash across Windows and POSIX separators.
        let posix = project_hash_from_root(&PathBuf::from("C:/Users/alice/repo"));
        let forward = project_hash_from_root(Path::new("C:/Users/alice/repo"));
        let backward = project_hash_from_root(Path::new("C:\\Users\\alice\\repo"));
        assert_eq!(posix, forward);
        assert_eq!(forward, backward);
    }

    #[test]
    fn project_hash_does_not_change_when_directory_is_created() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path().join("repo-that-does-not-exist-yet");

        let before = project_hash_from_root(&root);
        std::fs::create_dir(&root).expect("create project root");
        let after = project_hash_from_root(&root);

        assert_eq!(
            before, after,
            "same path must not re-hash after it starts existing"
        );
    }

    #[test]
    fn project_hash_normalises_dot_segments_without_filesystem_lookup() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path().join("missing-repo");
        let dotted = root.join(".").join("nested").join("..");

        assert_eq!(
            project_hash_from_root(&root),
            project_hash_from_root(&dotted),
            "lexically equivalent paths should share a project identity"
        );
    }

    #[test]
    fn project_hash_strips_windows_extended_prefix() {
        let normal = project_hash_from_root(Path::new("C:\\Users\\alice\\repo"));
        let extended = project_hash_from_root(Path::new("\\\\?\\C:\\Users\\alice\\repo"));

        assert_eq!(
            normal, extended,
            "Windows extended path prefix must not fork project identity"
        );
    }

    /// Verifies `table_counts` returns the right count on the happy path and
    /// surfaces a missing table as `Err` without poisoning the inventory.
    #[tokio::test]
    async fn table_counts_returns_per_table_results() {
        use sqlx::sqlite::SqlitePoolOptions;

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("open pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("apply migrations");
        sqlx::query!("INSERT INTO projects (id, name, path) VALUES ('p1', 'demo', '/tmp/demo')")
            .execute(&pool)
            .await
            .expect("seed projects");

        let results = table_counts(&pool, &["projects", "skills", "no_such_table"]).await;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, "projects");
        assert_eq!(results[0].1.as_ref().copied().ok(), Some(1));
        assert_eq!(results[1].0, "skills");
        assert_eq!(results[1].1.as_ref().copied().ok(), Some(0));
        assert_eq!(results[2].0, "no_such_table");
        assert!(
            results[2].1.is_err(),
            "missing table must surface as Err, got {:?}",
            results[2].1
        );
    }

    #[test]
    fn project_index_dir_uses_difflore_home() {
        // Assert `project_index_dir` lives under the shared test home,
        // proving the `DIFFLORE_HOME` plumbing without mutating the env.
        let home = shared_test_home();
        let dir = project_index_dir("abc123def456");
        assert!(
            dir.starts_with(home),
            "project_index_dir should live under DIFFLORE_HOME, got {dir:?}"
        );
        assert!(dir.ends_with(PathBuf::from("projects").join("abc123def456")));
    }
}
