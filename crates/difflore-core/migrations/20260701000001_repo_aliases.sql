-- Manual local project -> repo-scope mapping for checkouts without a supported
-- hosted git remote. This is device-local configuration, not rule provenance.

CREATE TABLE IF NOT EXISTS `repo_aliases` (
    `root_path`    TEXT NOT NULL,
    `project_hash` TEXT NOT NULL,
    `repo_scope`   TEXT NOT NULL,
    `source`       TEXT NOT NULL DEFAULT 'manual',
    `created_at`   TEXT DEFAULT (datetime('now')) NOT NULL,
    `updated_at`   TEXT DEFAULT (datetime('now')) NOT NULL,
    PRIMARY KEY (`project_hash`, `repo_scope`)
);

CREATE INDEX IF NOT EXISTS `idx_repo_aliases_root_path`
    ON `repo_aliases` (`root_path`);
