UPDATE `skills`
    SET `source_kind` = 'bot:unknown'
    WHERE `origin` = 'pr_review'
      AND `description` LIKE '%github.com/%'
      AND `source_kind` = 'human';
