CREATE TABLE IF NOT EXISTS `mcp_rule_serves` (
    `id`                         INTEGER PRIMARY KEY AUTOINCREMENT,
    `tool`                       TEXT NOT NULL,
    `session_id`                 TEXT,
    `repo_full_name`             TEXT,
    `file_path`                  TEXT,
    `query_hash`                 TEXT NOT NULL,
    `rule_ids_json`              TEXT NOT NULL DEFAULT '[]',
    `rule_count`                 INTEGER NOT NULL DEFAULT 0,
    `top_k`                      INTEGER NOT NULL DEFAULT 0,
    `was_empty`                  INTEGER NOT NULL DEFAULT 0,
    `strict_match_count`         INTEGER NOT NULL DEFAULT 0,
    `estimated_tokens`           INTEGER NOT NULL DEFAULT 0,
    `served_at`                  TEXT DEFAULT (datetime('now')) NOT NULL
);

CREATE INDEX IF NOT EXISTS `idx_mcp_rule_serves_tool_served`
    ON `mcp_rule_serves` (`tool`, `served_at`);

CREATE INDEX IF NOT EXISTS `idx_mcp_rule_serves_repo_file_served`
    ON `mcp_rule_serves` (`repo_full_name`, `file_path`, `served_at`);

CREATE INDEX IF NOT EXISTS `idx_mcp_rule_serves_query_hash`
    ON `mcp_rule_serves` (`query_hash`);
