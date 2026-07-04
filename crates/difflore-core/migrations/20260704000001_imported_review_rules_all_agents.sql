UPDATE `skills`
    SET `enabled_for_codex` = 1,
        `enabled_for_claude` = 1,
        `enabled_for_gemini` = 1,
        `enabled_for_cursor` = 1,
        `updated_at` = datetime('now')
    WHERE `source` = 'local'
      AND `origin` = 'pr_review'
      AND `status` IN ('active', 'pending')
      AND `captured_by_client` IN ('import-reviews', 'import-reviews:local-agent')
      AND (`enabled_for_codex` = 0
           OR `enabled_for_claude` = 0
           OR `enabled_for_gemini` = 0
           OR `enabled_for_cursor` = 0);
