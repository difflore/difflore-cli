-- Initial schema for DiffLore (AI Code Review + Fix Runner).
--
-- Tables: projects, providers, sessions (FK stub), skills, skill_repos,
--         review_items, review_comments, rule_examples, rule_events,
--         cloud_outbox, fix_outcomes, auth.
-- Index tables (code_chunks, rule_chunks, index_snapshots) live in a
-- separate index DB created programmatically by index_db.rs; they are
-- declared here as stubs so sqlx compile-time query checking works.

-- ── Projects ──

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

-- ── AI Provider configs ──

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

-- ── Sessions (FK stub — required by review_items FK) ──

CREATE TABLE IF NOT EXISTS `sessions` (
    `id`             TEXT PRIMARY KEY NOT NULL,
    `project_id`     TEXT NOT NULL,
    `workspace_path` TEXT NOT NULL DEFAULT '',
    `status`         TEXT DEFAULT 'running' NOT NULL,
    `started_at`     TEXT DEFAULT (datetime('now')) NOT NULL,
    `ended_at`       TEXT,
    FOREIGN KEY (`project_id`) REFERENCES `projects`(`id`) ON DELETE CASCADE
);

-- ── Skills / Rules ──
--
-- `file_patterns` is a JSON-encoded glob list (e.g. `["**/*.rs"]`).
-- NULL/empty means the rule applies everywhere. The strict file-pattern
-- cascade in local retrieval and cloud `recallByHybrid` uses this column.
--
-- `origin` records how the rule entered the system:
--   manual       — `difflore rules add` or hand-written SKILL.md
--   conversation — MCP `remember_rule` called from an agent chat
--   pr_review    — sedimented from a GitHub PR review comment
--   extracted    — cloud-side clustering surfaced a recurring fix pattern
-- Conversation-channel rules use a lower base confidence (0.6 vs 0.7).

CREATE TABLE IF NOT EXISTS `skills` (
    `id`                TEXT PRIMARY KEY NOT NULL,
    `name`              TEXT NOT NULL,
    `source`            TEXT NOT NULL,
    `directory`         TEXT NOT NULL,
    `version`           TEXT NOT NULL,
    `description`       TEXT DEFAULT '' NOT NULL,
    `type`              TEXT DEFAULT 'skill' NOT NULL,
    `engines`           TEXT DEFAULT '[]' NOT NULL,
    `tags`              TEXT DEFAULT '[]' NOT NULL,
    `trigger`           TEXT,
    `check_prompt`      TEXT,
    `repo_owner`        TEXT,
    `repo_name`         TEXT,
    `repo_branch`       TEXT,
    `readme_url`        TEXT,
    `source_repo`       TEXT,
    `enabled_for_codex`  INTEGER DEFAULT 0 NOT NULL,
    `enabled_for_claude` INTEGER DEFAULT 0 NOT NULL,
    `enabled_for_gemini` INTEGER DEFAULT 0 NOT NULL,
    `enabled_for_cursor` INTEGER DEFAULT 0 NOT NULL,
    `confidence_score`   REAL DEFAULT 0.7 NOT NULL,
    `file_patterns`     TEXT,
    `origin`            TEXT NOT NULL DEFAULT 'manual',
    `content_hash`      TEXT,
    `hash_created_at`   INTEGER,
    `status`            TEXT NOT NULL DEFAULT 'active'
        CHECK (`status` IN ('active', 'pending')),
    `installed_at`      TEXT DEFAULT (datetime('now')) NOT NULL,
    `updated_at`        TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_skills_origin` ON `skills` (`origin`);
CREATE INDEX IF NOT EXISTS `idx_skills_content_hash_created`
    ON `skills` (`content_hash`, `hash_created_at`);
CREATE INDEX IF NOT EXISTS `idx_skills_status` ON `skills` (`status`);

-- ── Skill repository sources ──

CREATE TABLE IF NOT EXISTS `skill_repos` (
    `id`         TEXT PRIMARY KEY NOT NULL,
    `owner`      TEXT NOT NULL,
    `name`       TEXT NOT NULL,
    `branch`     TEXT DEFAULT 'main' NOT NULL,
    `enabled`    INTEGER DEFAULT 1 NOT NULL,
    `created_at` TEXT DEFAULT (datetime('now')) NOT NULL
);

-- ── Review items ──

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

-- ── Review comments (human feedback) ──

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

-- ── Rule few-shot examples (bad_code / good_code pairs) ──

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

-- ── Rule event stream ──

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

-- ── Local Auto Fix outcome ledger ──

CREATE TABLE IF NOT EXISTS `fix_outcomes` (
    `id`            TEXT PRIMARY KEY NOT NULL,
    `rule_id`       TEXT,
    `rule_name`     TEXT NOT NULL,
    `file_path`     TEXT,
    `accepted`      INTEGER NOT NULL,
    `applied_ok`    INTEGER NOT NULL DEFAULT 0,
    `failed_reason` TEXT,
    `created_at`    TEXT DEFAULT (datetime('now')) NOT NULL,
    FOREIGN KEY (`rule_id`) REFERENCES `skills`(`id`) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_created`
    ON `fix_outcomes` (`created_at`);

CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_rule_created`
    ON `fix_outcomes` (`rule_id`, `created_at`);

-- ── Cloud upload outbox ──

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

-- ── Cloud auth tokens ──

CREATE TABLE IF NOT EXISTS `auth` (
    `key`   TEXT PRIMARY KEY NOT NULL,
    `value` TEXT NOT NULL
);

-- ── Index tables (compile-time validation stubs) ──
-- These tables are created programmatically in index_db.rs at runtime,
-- but must exist here for sqlx compile-time query checking.
--
-- `rule_chunks.file_patterns` is denormalized from `skills.file_patterns`
-- so the retrieval cascade can filter at retrieve time without joining
-- back to skills.

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

CREATE TABLE IF NOT EXISTS `rule_chunks` (
    `id`            TEXT PRIMARY KEY NOT NULL,
    `skill_id`      TEXT NOT NULL,
    `content`       TEXT NOT NULL,
    `embedding`     BLOB,
    `file_patterns` TEXT,
    `language`      TEXT,
    `repo_scope`    TEXT
);

CREATE TABLE IF NOT EXISTS `index_snapshots` (
    `project_id`  TEXT PRIMARY KEY NOT NULL,
    `file_count`  INTEGER DEFAULT 0 NOT NULL,
    `chunk_count` INTEGER DEFAULT 0 NOT NULL,
    `updated_at`  TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_code_chunks_project`
    ON `code_chunks` (`project_id`);

CREATE INDEX IF NOT EXISTS `idx_rule_chunks_skill`
    ON `rule_chunks` (`skill_id`);
