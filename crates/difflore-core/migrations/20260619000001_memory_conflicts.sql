-- Reviewable conflict records: persist deterministic candidate-vs-active rule
-- conflicts detected by the memory autopilot so a human can audit them later.
-- Snapshots of both sides are stored for auditability (the live rule/draft may
-- change or be deleted after detection). Mirrors the runtime CREATE TABLE guard
-- in ensure_memory_conflicts_table; both must stay in sync.
CREATE TABLE IF NOT EXISTS memory_conflicts (
    evidence_hash TEXT PRIMARY KEY,
    candidate_group_id TEXT NOT NULL DEFAULT '',
    candidate_rule_id TEXT,
    active_rule_id TEXT NOT NULL DEFAULT '',
    source_repo TEXT,
    overlap_basis TEXT NOT NULL DEFAULT '',
    candidate_title TEXT NOT NULL DEFAULT '',
    candidate_body TEXT NOT NULL DEFAULT '',
    active_title TEXT NOT NULL DEFAULT '',
    active_body TEXT NOT NULL DEFAULT '',
    candidate_patterns_json TEXT NOT NULL DEFAULT '[]',
    active_patterns_json TEXT NOT NULL DEFAULT '[]',
    llm_rationale TEXT,
    llm_confidence REAL,
    status TEXT NOT NULL DEFAULT 'detected',
    created_at TEXT DEFAULT (datetime('now')) NOT NULL,
    updated_at TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_memory_conflicts_status_updated
    ON memory_conflicts (status, updated_at);
