CREATE TABLE IF NOT EXISTS `review_gate_events` (
    `id`           INTEGER PRIMARY KEY AUTOINCREMENT,
    `rule_id`      TEXT NOT NULL,
    `file_path`    TEXT NOT NULL,
    `finding_hash` TEXT NOT NULL,
    `source`       TEXT NOT NULL CHECK (`source` IN ('review', 'ci')),
    `created_at`   TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_review_gate_events_created`
    ON `review_gate_events` (`created_at`);
CREATE INDEX IF NOT EXISTS `idx_review_gate_events_dedup_created`
    ON `review_gate_events` (`rule_id`, `file_path`, `finding_hash`, `created_at`);
