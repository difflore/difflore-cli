ALTER TABLE `skills`
    ADD COLUMN `source_kind` TEXT NOT NULL DEFAULT 'human';

CREATE INDEX IF NOT EXISTS `idx_skills_source_kind`
    ON `skills` (`source_kind`);
