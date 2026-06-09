-- ── Local rule recall telemetry ──
--
-- Records when a rule was surfaced by `difflore recall` so the memory
-- summary and `rules show` can highlight the most-used rules. Strictly
-- local — never uploaded to cloud (personal usage telemetry stays on
-- the device; cluster precision is computed cloud-side over consented
-- aggregates).

CREATE TABLE IF NOT EXISTS `rule_outcomes` (
    `id`         INTEGER PRIMARY KEY AUTOINCREMENT,
    `rule_id`    TEXT NOT NULL,
    `kind`       TEXT NOT NULL,
    `created_at` TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_kind_created`
    ON `rule_outcomes` (`kind`, `created_at`);

CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_rule_created`
    ON `rule_outcomes` (`rule_id`, `created_at`);
