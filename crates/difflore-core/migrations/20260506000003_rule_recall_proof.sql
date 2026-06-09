ALTER TABLE `rule_outcomes`
  ADD COLUMN `session_id` TEXT;

ALTER TABLE `rule_outcomes`
  ADD COLUMN `repo_full_name` TEXT;

ALTER TABLE `rule_outcomes`
  ADD COLUMN `file_path` TEXT;

ALTER TABLE `rule_outcomes`
  ADD COLUMN `query_hash` TEXT;

ALTER TABLE `rule_outcomes`
  ADD COLUMN `rank` INTEGER;

ALTER TABLE `rule_outcomes`
  ADD COLUMN `top_k` INTEGER;

ALTER TABLE `rule_outcomes`
  ADD COLUMN `strict_file_match` INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_rule_rank_created`
    ON `rule_outcomes` (`rule_id`, `rank`, `created_at`);

CREATE INDEX IF NOT EXISTS `idx_rule_outcomes_repo_file_created`
    ON `rule_outcomes` (`repo_full_name`, `file_path`, `created_at`);
