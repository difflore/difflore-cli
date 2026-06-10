#!/usr/bin/env bash
set -euo pipefail

mode="${1:-}"
if [[ "$mode" != "" && "$mode" != "--check" ]]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
db_path="$repo_root/.sqlx-prepare.db"
database_url="sqlite://$db_path"

cleanup() {
  rm -f "$db_path" "$db_path-shm" "$db_path-wal"
}
trap cleanup EXIT
cleanup

export SQLX_OFFLINE=false

cargo sqlx database create --database-url "$database_url"
cargo sqlx migrate run --source "$repo_root/crates/difflore-core/migrations" --database-url "$database_url"

prepare_args=()
if [[ "$mode" == "--check" ]]; then
  prepare_args+=(--check)
fi

(
  cd "$repo_root/crates/difflore-core"
  cargo sqlx prepare "${prepare_args[@]}" --database-url "$database_url" -- --all-targets
)

(
  cd "$repo_root/crates/difflore-cli"
  cargo sqlx prepare "${prepare_args[@]}" --database-url "$database_url" -- --all-targets
)
