-- Promote runtime-only schema bits into the baseline so sqlx::query! can
-- validate them at compile time.
--
-- Three additions:
--   1. `skills.cloud_id` — previously appended via a runtime ALTER in
--      team/cloud_id.rs; now part of the migration so the read/write
--      paths can use compile-time-checked macros.
--   2. `rule_index_meta` — exists on the per-project context-index DB,
--      stubbed here so `read_meta` / `write_meta` queries type-check.
--   3. `rule_chunks_fts` — FTS5 virtual table mirroring the per-project
--      context-index DB, stubbed for query!-level introspection.

ALTER TABLE `skills` ADD COLUMN `cloud_id` TEXT;
CREATE INDEX IF NOT EXISTS `idx_skills_cloud_id` ON `skills` (`cloud_id`);

CREATE TABLE IF NOT EXISTS `rule_index_meta` (
    `key`   TEXT PRIMARY KEY NOT NULL,
    `value` TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS `rule_chunks_fts` USING fts5(
    chunk_id UNINDEXED,
    content,
    tokenize='porter unicode61'
);
