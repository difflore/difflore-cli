CREATE TABLE IF NOT EXISTS accepted_edit_receipts (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  cloud_acceptance_id TEXT,
  local_receipt_key TEXT NOT NULL,
  repo_full_name TEXT,
  target_pr_number INTEGER,
  file_path TEXT,
  diff_signature TEXT NOT NULL,
  rule_ids_json TEXT NOT NULL DEFAULT '[]',
  acceptance_source TEXT NOT NULL,
  client TEXT,
  team_id TEXT,
  observations_inserted INTEGER NOT NULL DEFAULT 0,
  launch_grade INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  uploaded_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS accepted_edit_receipts_cloud_acceptance_idx
  ON accepted_edit_receipts(cloud_acceptance_id)
  WHERE cloud_acceptance_id IS NOT NULL AND cloud_acceptance_id <> '';

CREATE UNIQUE INDEX IF NOT EXISTS accepted_edit_receipts_local_key_idx
  ON accepted_edit_receipts(local_receipt_key);

CREATE INDEX IF NOT EXISTS accepted_edit_receipts_repo_idx
  ON accepted_edit_receipts(repo_full_name, uploaded_at);

CREATE INDEX IF NOT EXISTS accepted_edit_receipts_diff_idx
  ON accepted_edit_receipts(diff_signature, uploaded_at);
