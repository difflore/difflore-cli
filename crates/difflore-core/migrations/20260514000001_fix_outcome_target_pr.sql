ALTER TABLE `fix_outcomes`
  ADD COLUMN `repo_full_name` TEXT;

ALTER TABLE `fix_outcomes`
  ADD COLUMN `pr_number` INTEGER;

CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_repo_pr_created`
    ON `fix_outcomes` (`repo_full_name`, `pr_number`, `created_at`);
