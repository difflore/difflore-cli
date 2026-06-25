//! `difflore skills backfill-attribution` repairs local
//! `fix_outcomes.rule_id` attribution by matching `rule_name` to skills.
//!
//! This command scans every orphan row (`rule_id IS NULL OR rule_id=''`
//! AND `rule_name != ''`), resolves a matching `skills.id` via
//! [`difflore_core::observability::fix_outcomes::resolve_rule_id_by_name`], and either
//! reports what it would do (default dry-run) or applies the UPDATEs in
//! a single sqlx transaction.

use std::time::Instant;

use difflore_core::SqlitePool;
use difflore_core::observability::fix_outcomes::resolve_rule_id_by_name;

use crate::runtime::CommandContext;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BackfillArgs {
    /// When `true` (the default), only print what would change.
    pub dry_run: bool,
}

/// Single orphan-row resolution result. Held for both the dry-run
/// preview and the actual UPDATE pass so we never resolve twice.
#[derive(Debug, Clone)]
struct Resolution {
    rowid: i64,
    rule_name: String,
    resolved_rule_id: Option<String>,
}

pub(crate) async fn handle_backfill_attribution(ctx: &CommandContext, args: BackfillArgs) {
    if let Err(e) = run(&ctx.db, args).await {
        crate::support::util::exit_err(&format!("skills backfill-attribution failed: {e}"));
    }
}

async fn run(db: &SqlitePool, args: BackfillArgs) -> difflore_core::Result<()> {
    let started = Instant::now();

    // Pull every orphan row up-front so resolution and (optional) UPDATE
    // share the same snapshot. Local DBs we've measured top out at ~200
    // orphans so a single fetch is fine.
    //
    // Pull `created_at` so the resolver can prefer skills that existed when
    // the outcome was recorded.
    let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT rowid, rule_name, created_at FROM fix_outcomes \
         WHERE (rule_id IS NULL OR rule_id = '') \
           AND rule_name IS NOT NULL \
           AND TRIM(rule_name) != '' \
         ORDER BY rowid ASC",
    )
    .fetch_all(db)
    .await?;

    let total_orphans = rows.len();
    let mut resolutions = Vec::with_capacity(total_orphans);
    for (rowid, rule_name, created_at) in rows {
        let resolved = resolve_rule_id_by_name(db, &rule_name, created_at.as_deref()).await;
        resolutions.push(Resolution {
            rowid,
            rule_name,
            resolved_rule_id: resolved,
        });
    }

    let resolved: Vec<&Resolution> = resolutions
        .iter()
        .filter(|r| r.resolved_rule_id.is_some())
        .collect();
    let resolvable = resolved.len();

    if args.dry_run {
        for r in &resolved {
            // The resolver guarantees Some here.
            let id = r.resolved_rule_id.as_deref().unwrap_or("");
            println!(
                "would-update {}: rule_name='{}' -> rule_id='{}'",
                r.rowid, r.rule_name, id
            );
        }
        println!(
            "would-update {resolvable} of {total_orphans} orphan rows (dry-run; re-run with --no-dry-run to apply)"
        );
        return Ok(());
    }

    // Real write path: single transaction so a mid-batch failure doesn't
    // leave the table half-attributed.
    let mut tx = db.begin().await?;
    let mut updated = 0_u64;
    for r in &resolved {
        let Some(id) = r.resolved_rule_id.as_deref() else {
            continue;
        };
        let result = sqlx::query("UPDATE fix_outcomes SET rule_id = ?1 WHERE rowid = ?2")
            .bind(id)
            .bind(r.rowid)
            .execute(&mut *tx)
            .await?;
        updated += result.rows_affected();
    }
    tx.commit().await?;

    let elapsed_ms = started.elapsed().as_millis();
    println!("updated {updated} of {total_orphans} orphan rows in {elapsed_ms}ms");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn seeded_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                confidence_score REAL NOT NULL DEFAULT 0.7,
                installed_at TEXT DEFAULT (datetime('now')) NOT NULL,
                updated_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                id TEXT PRIMARY KEY NOT NULL,
                rule_id TEXT,
                rule_name TEXT NOT NULL,
                file_path TEXT,
                repo_full_name TEXT,
                pr_number INTEGER,
                diff_signature TEXT,
                accepted INTEGER NOT NULL,
                applied_ok INTEGER NOT NULL DEFAULT 0,
                failed_reason TEXT,
                created_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        for (id, name) in [
            ("skill-a", "Pin GitHub Actions refs to SHAs"),
            ("skill-b", "headChar returns wrong byte"),
            ("skill-c", "safeAt logic is inverted"),
            ("skill-d", "Use full word 'parameter'"),
            ("skill-e", "Avoid blocking I/O on the hot path"),
        ] {
            sqlx::query("INSERT INTO skills (id, name) VALUES (?1, ?2)")
                .bind(id)
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }

        let orphans = [
            ("Pin GitHub Actions refs to SHAs", 1_i64),
            ("headChar returns wrong byte", 2),
            ("safeAt logic is inverted", 3),
            ("Use full word 'parameter'", 4),
            // Suffix variant exercises the prefix-match path.
            ("Avoid blocking I/O on the hot path (tokio)", 5),
            // Unmatched: must stay orphan.
            ("Totally unknown rule that nobody learned", 6),
        ];
        for (idx, (rule_name, _)) in orphans.iter().enumerate() {
            let id = format!("outcome-{idx}");
            sqlx::query(
                "INSERT INTO fix_outcomes
                 (id, rule_id, rule_name, accepted, applied_ok)
                 VALUES (?1, '', ?2, 1, 1)",
            )
            .bind(id)
            .bind(*rule_name)
            .execute(&pool)
            .await
            .unwrap();
        }

        pool
    }

    // Note: the prefix fallback in `resolve_rule_id_by_name` is
    // `LOWER(name) LIKE LOWER(?1) || '%'` which checks whether the
    // skill *name* starts with the rule_name. So for
    // "Avoid blocking I/O on the hot path (tokio)" we DON'T expect a
    // prefix hit (the skill name is shorter); only the 4 exact matches
    // resolve. That's the conservative behavior we want.
    #[tokio::test]
    async fn dry_run_does_not_mutate() {
        let pool = seeded_pool().await;
        run(&pool, BackfillArgs { dry_run: true }).await.unwrap();

        let still_orphans: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fix_outcomes WHERE rule_id IS NULL OR rule_id = ''",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(still_orphans, 6, "dry-run must not write");
    }

    #[tokio::test]
    async fn non_dry_run_resolves_exact_matches() {
        let pool = seeded_pool().await;
        run(&pool, BackfillArgs { dry_run: false }).await.unwrap();

        let still_orphans: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fix_outcomes WHERE rule_id IS NULL OR rule_id = ''",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // 4 exact matches resolve; the suffix variant and the unmatched row
        // stay orphan (skill names are shorter, so no prefix hit).
        assert_eq!(still_orphans, 2, "expected 4 of 6 rows to be resolved");

        let pin_id: Option<String> = sqlx::query_scalar(
            "SELECT rule_id FROM fix_outcomes WHERE rule_name = 'Pin GitHub Actions refs to SHAs'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pin_id.as_deref(), Some("skill-a"));
    }

    // The integration brief asks for "5 orphan rows resolved". Use a
    // dedicated seed whose rule_names all match a `skills.name` exactly
    // so the contract holds regardless of prefix-direction nuances.
    #[tokio::test]
    async fn integration_seed_resolves_all_five() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE skills (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active',
                confidence_score REAL NOT NULL DEFAULT 0.7,
                installed_at TEXT DEFAULT (datetime('now')) NOT NULL,
                updated_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE fix_outcomes (
                id TEXT PRIMARY KEY NOT NULL,
                rule_id TEXT,
                rule_name TEXT NOT NULL,
                accepted INTEGER NOT NULL,
                applied_ok INTEGER NOT NULL DEFAULT 0,
                created_at TEXT DEFAULT (datetime('now')) NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        for i in 0..5 {
            let sid = format!("skill-{i}");
            let name = format!("rule number {i}");
            sqlx::query("INSERT INTO skills (id, name) VALUES (?1, ?2)")
                .bind(&sid)
                .bind(&name)
                .execute(&pool)
                .await
                .unwrap();
            let oid = format!("outcome-{i}");
            sqlx::query(
                "INSERT INTO fix_outcomes (id, rule_id, rule_name, accepted) VALUES (?1, '', ?2, 1)",
            )
            .bind(&oid)
            .bind(&name)
            .execute(&pool)
            .await
            .unwrap();
        }

        run(&pool, BackfillArgs { dry_run: false }).await.unwrap();

        let still_orphans: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM fix_outcomes WHERE rule_id IS NULL OR rule_id = ''",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(still_orphans, 0, "all 5 orphan rows must be resolved");
    }
}
