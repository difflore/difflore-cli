// SAFETY scope: a single `std::env::set_var` inside a `OnceLock` for the
// shared test-home tempdir. Gated to run exactly once per process.
#![allow(unsafe_code)]

use sha1::{Digest, Sha1};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use std::path::{Path, PathBuf};

/// Crate-wide singleton `TempDir` for the test process. Every test module
/// (`mcp_server`, startup, db, …) gets the SAME `DIFFLORE_HOME` by going
/// through this helper: it sets the env var exactly once and never
/// clears it, so a test that reads the env concurrently can't fall back
/// to the user's real `~/.difflore` and trash their data.
///
/// Gated by `#[cfg(test)]` so it never ships in release binaries; the
/// production code path below reads `DIFFLORE_HOME` directly as usual.
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
pub(crate) fn difflore_dir() -> Result<PathBuf, String> {
    // `DIFFLORE_HOME` lets integration tests redirect the data dir to a
    // tempdir without modifying $HOME / $USERPROFILE (which would race
    // against any other thread reading them). Honoured first; falls back
    // to the standard ~/.difflore in production.
    if let Some(custom) = crate::env::difflore_home() {
        return Ok(PathBuf::from(custom));
    }
    // In test binaries, never trust a missing `DIFFLORE_HOME` — some
    // sibling test may have mid-flight `remove_var`'d it, and the last
    // thing we want is for a concurrent `init_db()` to silently fall
    // through to the developer's real `~/.difflore` and race `migrate!`
    // against their actual data. Route to the crate-wide
    // `shared_test_home()` instead, so every test ends up in the same
    // tempdir regardless of whether anyone's holding ENV_LOCK.
    #[cfg(test)]
    {
        Ok(shared_test_home().to_path_buf())
    }
    #[cfg(not(test))]
    Ok(dirs::home_dir()
        .ok_or_else(|| "cannot resolve home directory".to_owned())?
        .join(".difflore"))
}

/// Path to the global data.db — stays global (cross-project features like
/// `rules stats` rely on a single aggregate view). Only the per-project
/// embedding index moves out of the global root.
pub fn data_db_path() -> Result<PathBuf, String> {
    Ok(difflore_dir()?.join("data.db"))
}

/// Derive a stable hash for a project root. Uses SHA-1 of a purely
/// lexical, slash-normalised path identity, hex, truncated to 12 chars
/// (48 bits).
/// It intentionally does not call `canonicalize()`: project identity must
/// not change just because a directory was created/deleted, or because
/// Windows returned an extended `\\?\` path on one call and a normal path
/// on another. A pair of distinct roots collides with probability 2^-48;
/// cumulative risk grows with the number of project roots under the usual
/// birthday bound, so this is suitable for local DB partition names, not as
/// a security boundary.
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

/// Resolve the current project root: `git rev-parse --show-toplevel` in
/// the current working directory, or the cwd itself if that fails (not a
/// git repo, git not installed, etc.). Never panics; falls back to `.`
/// when even `current_dir` errors.
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
/// Does not create the directory — callers that need the path on disk
/// are responsible for `create_dir_all`.
pub fn project_index_dir(project_hash: &str) -> PathBuf {
    let mut p = difflore_dir().unwrap_or_else(|_| PathBuf::from(".difflore"));
    p.push("projects");
    p.push(project_hash);
    p
}

/// Process-wide async lock around `sqlx::migrate!`. Without this, two
/// tokio runtimes concurrently calling the migrate runner (common in
/// the test suite) would both try to acquire sqlx's
/// `_sqlx_migrations` row lock and intermittently fail with
/// `migration failed: while executing migrations: …`. The lock only
/// covers the migration step, not the pool itself — the `SqlitePool` is
/// `Clone` via internal `Arc`, so the cost of holding this lock is a
/// few ms per unique call site.
static MIGRATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Run every pending migration from `./migrations` against the given pool.
///
/// Centralised here so the `sqlx::migrate!` macro is expanded exactly once
/// per crate and every migration path is guarded by `MIGRATION_LOCK`.
pub async fn run_migrations(pool: &SqlitePool) -> Result<(), String> {
    let _guard = MIGRATION_LOCK.lock().await;
    // `sqlx::migrate!` embeds the migration files into the binary at
    // compile time so `cargo install difflore-cli` doesn't need the
    // user's `~/.cargo/registry/src/` to persist post-install. The
    // earlier `Migrator::new(path)` form read migrations from disk
    // at runtime via `env!("CARGO_MANIFEST_DIR")`, which broke if
    // the user ever ran `cargo cache clean`.
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|e| format!("migration failed: {e}"))
}

/// Cache of opened `data.db` pools keyed by resolved path. Callers
/// (MCP server, hooks, CLI subcommands, startup probe) all share the
/// same pool per DB file instead of each opening an independent WAL
/// connection and racing on migrations. Using `tokio::sync::Mutex`
/// (rather than `std::sync::Mutex`) is important: the whole open +
/// migrate pipeline is `await`-heavy, and we want the critical
/// section held across those awaits so a second concurrent caller
/// sees the finished pool on cache hit, not a half-initialised DB.
/// `SqlitePool` is `Clone` (internal `Arc`) so cache hits are free.
static POOL_CACHE: tokio::sync::Mutex<Option<std::collections::HashMap<PathBuf, SqlitePool>>> =
    tokio::sync::Mutex::const_new(None);

/// Best-effort: restrict a path created under `~/.difflore` to the current user
/// on Unix (dir → 0700, file → 0600). The local SQLite stores hold the cloud
/// auth token (encrypted) and the user's imported review data; a 0600/0700
/// posture keeps them off other users on a shared host. On Windows the per-user
/// profile directory is already ACL-restricted to the owner, so this is a
/// no-op. Failures are ignored — perms are hardening, not correctness.
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

pub async fn init_db() -> Result<SqlitePool, String> {
    let dir = difflore_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create ~/.difflore: {e}"))?;
    restrict_to_owner(&dir, true);
    let db_path = dir.join("data.db");

    // Hold the cache lock across the whole open+migrate flow so only
    // one caller per DB path runs migrations. Concurrent callers wait
    // here and get the finished pool on the second pass. `.await`
    // inside the guard is fine — `tokio::sync::Mutex` supports it.
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
            // A WAL database can't be opened on a read-only home (it must
            // create/write the `-shm` / `-wal` sidecars), which surfaces as
            // SQLITE_CANTOPEN. The common cause is a sandboxed MCP client
            // (e.g. codex with a restrictive `--sandbox`) confining writes to
            // the workspace. Turn the cryptic SQLite code into an actionable
            // hint; opening such a database read-only is not currently
            // supported (existing `-wal` defeats `immutable=1`).
            if is_readonly_home_open_error(&e) {
                format!(
                    "failed to open data.db: {e}\n\
                     hint: ~/.difflore appears read-only. A sandboxed agent (e.g. codex with a \
                     restrictive --sandbox) blocks DiffLore's writes. Run DiffLore unsandboxed for \
                     that client, or set DIFFLORE_HOME to a writable path."
                )
            } else {
                format!("failed to open data.db: {e}")
            }
        })?;

    // The DB + its WAL/SHM sidecars now exist; lock them to the owner (Unix).
    restrict_sqlite_files(&db_path);

    run_migrations(&pool).await?;

    cache.insert(db_path.clone(), pool.clone());
    Ok(pool)
}

/// True when a SQLite open failure indicates a read-only data home — typically
/// a sandboxed MCP client (codex etc.) that blocks the `-shm`/`-wal` writes a
/// WAL database needs, surfacing as `SQLITE_CANTOPEN` / `SQLITE_READONLY`.
fn is_readonly_home_open_error(err: &sqlx::Error) -> bool {
    let s = err.to_string().to_ascii_lowercase();
    s.contains("code: 14") // SQLITE_CANTOPEN
        || s.contains("code: 8") // SQLITE_READONLY
        || s.contains("unable to open database file")
        || s.contains("readonly")
        || s.contains("read-only")
}

/// Count rows in the named tables. Used by `difflore doctor` to snapshot
/// store size without leaking `SqlitePool` to the CLI crate. Tables that
/// don't exist (e.g. on a fresh install before migrations) surface as
/// `Err(message)` rather than aborting — the doctor report still wants
/// to show a best-effort inventory.
pub async fn table_counts(
    pool: &SqlitePool,
    tables: &[&str],
) -> Vec<(String, Result<i64, String>)> {
    let mut out = Vec::with_capacity(tables.len());
    for t in tables {
        let count: Result<i64, String> = match *t {
            "skills" => sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM skills"#)
                .fetch_one(pool)
                .await
                .map_err(|e| e.to_string()),
            "review_items" => {
                sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM review_items"#)
                    .fetch_one(pool)
                    .await
                    .map_err(|e| e.to_string())
            }
            "review_comments" => {
                sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM review_comments"#)
                    .fetch_one(pool)
                    .await
                    .map_err(|e| e.to_string())
            }
            "providers" => sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM providers"#)
                .fetch_one(pool)
                .await
                .map_err(|e| e.to_string()),
            "cloud_outbox" => {
                sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM cloud_outbox"#)
                    .fetch_one(pool)
                    .await
                    .map_err(|e| e.to_string())
            }
            "projects" => sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM projects"#)
                .fetch_one(pool)
                .await
                .map_err(|e| e.to_string()),
            other => Err(format!("unknown table: {other}")),
        };
        out.push((t.to_string(), count));
    }
    out
}

/// Aggregate snapshot of the local skills (rules) corpus for
/// `difflore doctor --report`. Reports total count, breakdown by
/// `origin` and top `source_repo` partitions, and the count of skills
/// with empty `file_patterns` (recall-killing signature). Empty
/// `file_patterns` once tripped Eval-26 — keeping a permanent counter
/// catches future cluster-pipeline regressions.
#[derive(Debug, Default)]
pub struct CorpusHealth {
    pub total: i64,
    pub by_origin: Vec<(String, i64)>,
    pub by_source_repo: Vec<(String, i64)>,
    pub empty_file_patterns: i64,
}

pub async fn corpus_health(pool: &SqlitePool) -> Result<CorpusHealth, String> {
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

    // `source_repo` is the single provenance column for current rule
    // attribution. Retired `repo_owner` / `repo_name` fields are not
    // interpreted as a source repo.
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
        // Same logical path on Windows vs POSIX must collapse to the same
        // hash — we replace `\` with `/` before hashing, so both slash
        // variants and identical strings via different APIs all match.
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

    /// Safety net for `table_counts` — the only remaining non-macro SQL
    /// site in this module (table name is interpolated, not bindable).
    /// Verifies happy path returns the right count and a missing table
    /// surfaces as `Err` instead of poisoning the rest of the inventory.
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
        // The shared test home sets `DIFFLORE_HOME` once per process.
        // Asserting that `project_index_dir` starts under that path
        // proves the env-var plumbing without us mutating the env at
        // all — which is what used to race against mcp_server and
        // startup tests running in parallel (they'd fall back to
        // `~/.difflore` after another test called `remove_var`).
        let home = shared_test_home();
        let dir = project_index_dir("abc123def456");
        assert!(
            dir.starts_with(home),
            "project_index_dir should live under DIFFLORE_HOME, got {dir:?}"
        );
        assert!(dir.ends_with(PathBuf::from("projects").join("abc123def456")));
    }
}
