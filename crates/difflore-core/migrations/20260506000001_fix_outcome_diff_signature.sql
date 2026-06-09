ALTER TABLE `fix_outcomes`
  ADD COLUMN `diff_signature` TEXT;

CREATE INDEX IF NOT EXISTS `idx_fix_outcomes_signature_created`
    ON `fix_outcomes` (`diff_signature`, `created_at`);
