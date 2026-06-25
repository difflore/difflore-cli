-- Initial schema for DiffLore local storage.

CREATE TABLE IF NOT EXISTS `projects` (
    `id`              TEXT PRIMARY KEY NOT NULL,
    `name`            TEXT NOT NULL,
    `path`            TEXT NOT NULL,
    `git_branch`      TEXT,
    `active_sessions` INTEGER DEFAULT 0 NOT NULL,
    `created_at`      TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS `projects_path_unique`
    ON `projects` (`path`);

CREATE TABLE IF NOT EXISTS `providers` (
    `id`            TEXT PRIMARY KEY NOT NULL,
    `name`          TEXT NOT NULL,
    `base_url`      TEXT NOT NULL,
    `api_key`       TEXT NOT NULL,
    `model_mapping` TEXT DEFAULT '{}' NOT NULL,
    `is_active`     INTEGER DEFAULT 0 NOT NULL,
    `created_at`    TEXT DEFAULT (datetime('now')) NOT NULL,
    `updated_at`    TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE TABLE IF NOT EXISTS `sessions` (
    `id`             TEXT PRIMARY KEY NOT NULL,
    `project_id`     TEXT NOT NULL,
    `workspace_path` TEXT NOT NULL DEFAULT '',
    `status`         TEXT DEFAULT 'running' NOT NULL,
    `started_at`     TEXT DEFAULT (datetime('now')) NOT NULL,
    `ended_at`       TEXT,
    FOREIGN KEY (`project_id`) REFERENCES `projects`(`id`) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS `skills` (
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
        CHECK (`status` IN ('active', 'pending')),
    `installed_at`       TEXT DEFAULT (datetime('now')) NOT NULL,
    `updated_at`         TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_skills_origin` ON `skills` (`origin`);
CREATE INDEX IF NOT EXISTS `idx_skills_content_hash_created`
    ON `skills` (`content_hash`, `hash_created_at`);
CREATE INDEX IF NOT EXISTS `idx_skills_status` ON `skills` (`status`);
CREATE INDEX IF NOT EXISTS `idx_skills_cloud_id` ON `skills` (`cloud_id`);

CREATE TABLE IF NOT EXISTS `rejected_signatures` (
    `content_hash` TEXT NOT NULL,
    `source_repo`  TEXT,
    `comment_url`  TEXT,
    `reason`       TEXT,
    `rejected_at`  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (`content_hash`)
);

CREATE TABLE IF NOT EXISTS `skill_repos` (
    `id`         TEXT PRIMARY KEY NOT NULL,
    `owner`      TEXT NOT NULL,
    `name`       TEXT NOT NULL,
    `branch`     TEXT DEFAULT 'main' NOT NULL,
    `enabled`    INTEGER DEFAULT 1 NOT NULL,
    `created_at` TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE TABLE IF NOT EXISTS `review_items` (
    `id`                 TEXT PRIMARY KEY NOT NULL,
    `session_id`         TEXT,
    `project_id`         TEXT,
    `file_path`          TEXT NOT NULL,
    `diff_content`       TEXT DEFAULT '' NOT NULL,
    `status`             TEXT DEFAULT 'pending' NOT NULL,
    `source`             TEXT DEFAULT 'local' NOT NULL,
    `source_kind`        TEXT DEFAULT 'session_local' NOT NULL,
    `external_review_id` TEXT,
    `repo_full_name`     TEXT,
    `pr_number`          INTEGER,
    `author`             TEXT,
    `synced_at`          TEXT,
    `metadata`           TEXT,
    `created_at`         TEXT DEFAULT (datetime('now')) NOT NULL,
    `reviewed_at`        TEXT,
    FOREIGN KEY (`session_id`) REFERENCES `sessions`(`id`) ON DELETE CASCADE,
    FOREIGN KEY (`project_id`) REFERENCES `projects`(`id`) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS `idx_review_items_external_review_id`
    ON `review_items` (`external_review_id`) WHERE `external_review_id` IS NOT NULL;

CREATE TABLE IF NOT EXISTS `review_comments` (
    `id`                  TEXT PRIMARY KEY NOT NULL,
    `review_item_id`      TEXT NOT NULL,
    `external_comment_id` TEXT,
    `line_number`         INTEGER NOT NULL,
    `content`             TEXT NOT NULL,
    `author`              TEXT,
    `comment_url`         TEXT,
    `thread_id`           TEXT,
    `metadata`            TEXT,
    `created_at`          TEXT DEFAULT (datetime('now')) NOT NULL,
    FOREIGN KEY (`review_item_id`) REFERENCES `review_items`(`id`) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS `idx_review_comments_external_comment_id`
    ON `review_comments` (`external_comment_id`) WHERE `external_comment_id` IS NOT NULL;

CREATE TABLE IF NOT EXISTS `rule_examples` (
    `id`          TEXT PRIMARY KEY NOT NULL,
    `skill_id`    TEXT NOT NULL,
    `bad_code`    TEXT NOT NULL,
    `good_code`   TEXT NOT NULL,
    `description` TEXT,
    `source`      TEXT DEFAULT 'manual' NOT NULL,
    `created_at`  TEXT DEFAULT (datetime('now')) NOT NULL,
    FOREIGN KEY (`skill_id`) REFERENCES `skills`(`id`) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS `idx_rule_examples_skill`
    ON `rule_examples` (`skill_id`);

CREATE TABLE IF NOT EXISTS `rule_events` (
    `id`                TEXT PRIMARY KEY NOT NULL,
    `skill_id`          TEXT NOT NULL,
    `kind`              TEXT NOT NULL,
    `source`            TEXT DEFAULT 'local_feedback' NOT NULL,
    `confidence_before` REAL,
    `confidence_after`  REAL,
    `reason`            TEXT,
    `metadata`          TEXT,
    `created_at`        TEXT DEFAULT (datetime('now')) NOT NULL,
    FOREIGN KEY (`skill_id`) REFERENCES `skills`(`id`) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS `idx_rule_events_skill_created`
    ON `rule_events` (`skill_id`, `created_at`);

CREATE TABLE IF NOT EXISTS `fix_outcomes` (
    `id`             TEXT PRIMARY KEY NOT NULL,
    `rule_id`        TEXT,
    `rule_name`      TEXT NOT NULL,
    `file_path`      TEXT,
    `repo_full_name` TEXT,
    `pr_number`      INTEGER,
    `diff_signature` TEXT,
    `accepted`       INTEGER NOT NULL,
    `applied_ok`     INTEGER NOT NULL DEFAULT 0,
    `failed_reason`  TEXT,
    `created_at`     TEXT DEFAULT (datetime('now')) NOT NULL,
    FOREIGN KEY (`rule_id`) REFERENCES `skills`(`id`) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_created`
    ON `fix_outcomes` (`created_at`);
CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_rule_created`
    ON `fix_outcomes` (`rule_id`, `created_at`);
CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_signature_created`
    ON `fix_outcomes` (`diff_signature`, `created_at`);
CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_repo_pr_created`
    ON `fix_outcomes` (`repo_full_name`, `pr_number`, `created_at`);

CREATE TABLE IF NOT EXISTS `cloud_outbox` (
    `id`           INTEGER PRIMARY KEY AUTOINCREMENT,
    `kind`         TEXT NOT NULL,
    `payload_json` TEXT NOT NULL,
    `status`       TEXT NOT NULL DEFAULT 'pending',
    `retry_count`  INTEGER NOT NULL DEFAULT 0,
    `created_at`   INTEGER NOT NULL,
    `claimed_at`   INTEGER,
    `last_error`   TEXT,
    `enriched_at`  INTEGER
);

CREATE INDEX IF NOT EXISTS `idx_cloud_outbox_status_created`
    ON `cloud_outbox` (`status`, `created_at`);
CREATE INDEX IF NOT EXISTS `idx_cloud_outbox_enrich`
    ON `cloud_outbox` (`kind`, `status`, `enriched_at`)
    WHERE `kind` = 'observation' AND `enriched_at` IS NULL;

CREATE TABLE IF NOT EXISTS `auth` (
    `key`   TEXT PRIMARY KEY NOT NULL,
    `value` TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS `code_chunks` (
    `id`         TEXT PRIMARY KEY NOT NULL,
    `project_id` TEXT NOT NULL,
    `file_path`  TEXT NOT NULL,
    `start_line` INTEGER NOT NULL,
    `end_line`   INTEGER NOT NULL,
    `content`    TEXT NOT NULL,
    `language`   TEXT NOT NULL,
    `embedding`  BLOB,
    `updated_at` TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_code_chunks_project`
    ON `code_chunks` (`project_id`);

CREATE TABLE IF NOT EXISTS `rule_chunks` (
    `id`            TEXT PRIMARY KEY NOT NULL,
    `skill_id`      TEXT NOT NULL,
    `content`       TEXT NOT NULL,
    `embedding`     BLOB,
    `file_patterns` TEXT,
    `language`      TEXT,
    `repo_scope`    TEXT
);

CREATE INDEX IF NOT EXISTS `idx_rule_chunks_skill`
    ON `rule_chunks` (`skill_id`);

CREATE TABLE IF NOT EXISTS `index_snapshots` (
    `project_id`  TEXT PRIMARY KEY NOT NULL,
    `file_count`  INTEGER DEFAULT 0 NOT NULL,
    `chunk_count` INTEGER DEFAULT 0 NOT NULL,
    `updated_at`  TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE TABLE IF NOT EXISTS `rule_index_meta` (
    `key`   TEXT PRIMARY KEY NOT NULL,
    `value` TEXT NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS `rule_chunks_fts` USING fts5(
    chunk_id UNINDEXED,
    content,
    tokenize='porter unicode61'
);

CREATE TABLE IF NOT EXISTS `rule_outcomes` (
    `id`                INTEGER PRIMARY KEY AUTOINCREMENT,
    `rule_id`           TEXT NOT NULL,
    `kind`              TEXT NOT NULL,
    `session_id`        TEXT,
    `repo_full_name`    TEXT,
    `file_path`         TEXT,
    `query_hash`        TEXT,
    `rank`              INTEGER,
    `top_k`             INTEGER,
    `strict_file_match` INTEGER NOT NULL DEFAULT 0,
    `created_at`        TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_kind_created`
    ON `rule_outcomes` (`kind`, `created_at`);
CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_rule_created`
    ON `rule_outcomes` (`rule_id`, `created_at`);
CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_rule_rank_created`
    ON `rule_outcomes` (`rule_id`, `rank`, `created_at`);
CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_repo_file_created`
    ON `rule_outcomes` (`repo_full_name`, `file_path`, `created_at`);

CREATE TABLE IF NOT EXISTS `mcp_rule_serves` (
    `id`                 INTEGER PRIMARY KEY AUTOINCREMENT,
    `tool`               TEXT NOT NULL,
    `session_id`         TEXT,
    `repo_full_name`     TEXT,
    `file_path`          TEXT,
    `query_hash`         TEXT NOT NULL,
    `rule_ids_json`      TEXT NOT NULL DEFAULT '[]',
    `rule_count`         INTEGER NOT NULL DEFAULT 0,
    `top_k`              INTEGER NOT NULL DEFAULT 0,
    `was_empty`          INTEGER NOT NULL DEFAULT 0,
    `strict_match_count` INTEGER NOT NULL DEFAULT 0,
    `estimated_tokens`   INTEGER NOT NULL DEFAULT 0,
    `served_at`          TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_mcp_rule_serves_tool_served`
    ON `mcp_rule_serves` (`tool`, `served_at`);
CREATE INDEX IF NOT EXISTS `idx_mcp_rule_serves_repo_file_served`
    ON `mcp_rule_serves` (`repo_full_name`, `file_path`, `served_at`);
CREATE INDEX IF NOT EXISTS `idx_mcp_rule_serves_query_hash`
    ON `mcp_rule_serves` (`query_hash`);

CREATE TABLE IF NOT EXISTS `memory_autopilot_schedule` (
    `id`                    INTEGER PRIMARY KEY CHECK (`id` = 1),
    `enabled`               INTEGER NOT NULL DEFAULT 1,
    `dirty`                 INTEGER NOT NULL DEFAULT 0,
    `last_dirty_at`         TEXT,
    `last_dirty_reason`     TEXT,
    `last_trigger_at`       TEXT,
    `last_trigger_reason`   TEXT,
    `last_run_at`           TEXT,
    `lease_owner`           TEXT,
    `lease_expires_at`      TEXT,
    `last_result`           TEXT NOT NULL DEFAULT '{}',
    `trigger_count`         INTEGER NOT NULL DEFAULT 0,
    `dirty_mark_count`      INTEGER NOT NULL DEFAULT 0,
    `spawn_attempt_count`   INTEGER NOT NULL DEFAULT 0,
    `spawn_success_count`   INTEGER NOT NULL DEFAULT 0,
    `run_count`             INTEGER NOT NULL DEFAULT 0,
    `productive_run_count`  INTEGER NOT NULL DEFAULT 0,
    `skip_count`            INTEGER NOT NULL DEFAULT 0,
    `last_skip_reason`      TEXT
);

INSERT OR IGNORE INTO `memory_autopilot_schedule` (`id`) VALUES (1);

CREATE TABLE IF NOT EXISTS `memory_autopilot_events` (
    `id`            INTEGER PRIMARY KEY AUTOINCREMENT,
    `event_type`    TEXT NOT NULL,
    `rule_id`       TEXT,
    `item_ids_json` TEXT NOT NULL DEFAULT '[]',
    `group_id`      TEXT,
    `title`         TEXT NOT NULL DEFAULT '',
    `reason`        TEXT NOT NULL DEFAULT '',
    `payload_json`  TEXT NOT NULL DEFAULT '{}',
    `created_at`    TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE TABLE IF NOT EXISTS `memory_curator_recommendations` (
    `group_id`           TEXT PRIMARY KEY,
    `input_hash`         TEXT NOT NULL,
    `state`              TEXT NOT NULL,
    `confidence`         TEXT,
    `title`              TEXT NOT NULL DEFAULT '',
    `rule`               TEXT NOT NULL DEFAULT '',
    `file_patterns_json` TEXT NOT NULL DEFAULT '[]',
    `reason`             TEXT NOT NULL DEFAULT '',
    `item_ids_json`      TEXT NOT NULL DEFAULT '[]',
    `prompt_version`     TEXT NOT NULL,
    `created_at`         TEXT DEFAULT (datetime('now')) NOT NULL,
    `updated_at`         TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_memory_curator_recommendations_state_updated`
    ON `memory_curator_recommendations` (`state`, `updated_at`);
