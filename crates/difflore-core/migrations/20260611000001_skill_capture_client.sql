-- Track which local agent/client captured a rule without changing origin.
ALTER TABLE `skills` ADD COLUMN `captured_by_client` TEXT;
