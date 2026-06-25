use super::*;

pub async fn load_memory_digest(pool: &SqlitePool, limit: usize) -> Result<MemoryDigest> {
    let plan = build_plan(
        pool,
        normalize_limit(limit),
        BuildPlanOptions {
            local_ai_curator: false,
            curator_max_candidates: None,
        },
    )
    .await?;
    Ok(plan.digest)
}
