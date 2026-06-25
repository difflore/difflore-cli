/// Read the most-recent `window` audit runs, ordered by `ts_ms`.
/// Returns Err on any corrupt JSONL line so a damaged history file surfaces
/// instead of being treated as "no activity".
pub(crate) fn load_audit_history(
    window: usize,
) -> anyhow::Result<Vec<difflore_core::context::intent_filter::AuditRunRecord>> {
    use difflore_core::context::intent_filter::AuditRunRecord;

    let Some(path) = audit_history_path() else {
        return Ok(Vec::new());
    };
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
    };
    let mut all: Vec<AuditRunRecord> = Vec::new();
    let mut corrupt = 0usize;
    let lines = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let last_index = lines.len().saturating_sub(1);
    let final_line_may_be_torn = !body.ends_with('\n');
    for (index, line) in lines.into_iter().enumerate() {
        match serde_json::from_str::<AuditRunRecord>(line) {
            Ok(record) => all.push(record),
            Err(_) if final_line_may_be_torn && index == last_index => {}
            Err(_) => corrupt += 1,
        }
    }
    if corrupt > 0 {
        return Err(anyhow::anyhow!(
            "audit history at {} has {corrupt} corrupt line(s)",
            path.display()
        ));
    }
    all.sort_by_key(|record| record.ts_ms);
    if all.len() > window {
        all.drain(..all.len() - window);
    }
    Ok(all)
}

fn audit_history_path() -> Option<std::path::PathBuf> {
    difflore_core::infra::paths::data_home()
        .ok()
        .map(|dir| dir.join("audit-history.jsonl"))
}
