-- no-transaction
-- Allow active memory rules to be paused without turning them back into
-- pending drafts. SQLite cannot alter CHECK constraints in place, so rebuild
-- the skills table with the widened status enum.

PRAGMA foreign_keys = OFF;

DROP TABLE IF EXISTS `skills_new`;

CREATE TABLE `skills_new` (
    `id`                 TEXT PRIMARY KEY NOT NULL,
    `name`               TEXT NOT NULL,
    `source`             TEXT NOT NULL,
    `directory`          TEXT NOT NULL,
    `version`            TEXT NOT NULL,
    `description`        TEXT DEFAULT '' NOT NULL,
    `type`               TEXT DEFAULT 'skill' NOT NULL,
    `engines`            TEXT DEFAULT '[]' NOT NULL,
    `tags`               TEXT DEFAULT '[]' NOT NULL,
    `trigger`            TEXT,
    `check_prompt`       TEXT,
    `repo_owner`         TEXT,
    `repo_name`          TEXT,
    `repo_branch`        TEXT,
    `readme_url`         TEXT,
    `source_repo`        TEXT,
    `enabled_for_codex`  INTEGER DEFAULT 0 NOT NULL,
    `enabled_for_claude` INTEGER DEFAULT 0 NOT NULL,
    `enabled_for_gemini` INTEGER DEFAULT 0 NOT NULL,
    `enabled_for_cursor` INTEGER DEFAULT 0 NOT NULL,
    `confidence_score`   REAL DEFAULT 0.7 NOT NULL,
    `file_patterns`      TEXT,
    `origin`             TEXT NOT NULL DEFAULT 'manual',
    `content_hash`       TEXT,
    `hash_created_at`    INTEGER,
    `cloud_id`           TEXT,
    `captured_by_client` TEXT,
    `status`             TEXT NOT NULL DEFAULT 'active'
        CHECK (`status` IN ('active', 'pending', 'disabled')),
    `installed_at`       TEXT DEFAULT (datetime('now')) NOT NULL,
    `updated_at`         TEXT DEFAULT (datetime('now')) NOT NULL
);

INSERT INTO `skills_new` (
    `id`,
    `name`,
    `source`,
    `directory`,
    `version`,
    `description`,
    `type`,
    `engines`,
    `tags`,
    `trigger`,
    `check_prompt`,
    `repo_owner`,
    `repo_name`,
    `repo_branch`,
    `readme_url`,
    `source_repo`,
    `enabled_for_codex`,
    `enabled_for_claude`,
    `enabled_for_gemini`,
    `enabled_for_cursor`,
    `confidence_score`,
    `file_patterns`,
    `origin`,
    `content_hash`,
    `hash_created_at`,
    `cloud_id`,
    `captured_by_client`,
    `status`,
    `installed_at`,
    `updated_at`
)
SELECT
    `id`,
    `name`,
    `source`,
    `directory`,
    `version`,
    `description`,
    `type`,
    `engines`,
    `tags`,
    `trigger`,
    `check_prompt`,
    `repo_owner`,
    `repo_name`,
    `repo_branch`,
    `readme_url`,
    `source_repo`,
    `enabled_for_codex`,
    `enabled_for_claude`,
    `enabled_for_gemini`,
    `enabled_for_cursor`,
    `confidence_score`,
    `file_patterns`,
    `origin`,
    `content_hash`,
    `hash_created_at`,
    `cloud_id`,
    `captured_by_client`,
    `status`,
    `installed_at`,
    `updated_at`
FROM `skills`;

DROP TABLE `skills`;
ALTER TABLE `skills_new` RENAME TO `skills`;

CREATE INDEX IF NOT EXISTS `idx_skills_origin` ON `skills` (`origin`);
CREATE INDEX IF NOT EXISTS `idx_skills_content_hash_created`
    ON `skills` (`content_hash`, `hash_created_at`);
CREATE INDEX IF NOT EXISTS `idx_skills_status` ON `skills` (`status`);
CREATE INDEX IF NOT EXISTS `idx_skills_cloud_id` ON `skills` (`cloud_id`);

PRAGMA foreign_keys = ON;
