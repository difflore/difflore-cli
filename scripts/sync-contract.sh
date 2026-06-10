#!/usr/bin/env bash
# Cross-repo contract sync for the vendored OpenAPI spec.
#
# The cloud repo (difflore-cloud) is the source of truth: `pnpm contract:export`
# instantiates the OpenAPIGenerator offline and writes the full /api spec to
# src/contracts/openapi/api.json, which is committed there. The CLI vendors a
# copy at crates/difflore-core/contracts/openapi-spec.json plus a SOURCE file
# pinning the cloud commit + sha256, because `openapi_contract::generate_types!`
# reads the vendored copy at compile time.
#
# Two modes, chosen automatically:
#
#   --check (CI mode)
#       Verify the vendored spec's sha256 still matches the value recorded in
#       SOURCE. Does not touch the cloud repo. Exits non-zero on drift so a
#       hand-edit of openapi-spec.json without a SOURCE refresh turns CI red.
#       This is the gate referenced in blueprint section 5.2.
#
#   (default, sync mode)
#       Locate the cloud spec (sibling checkout by default; override with
#       --cloud-repo), then DECIDE:
#
#         A. Direct adoption — only when the cloud spec is structurally
#            compatible AND adopting it would not regress the generated types
#            (i.e. the normalized cloud spec is a structural superset: every
#            property/required field the vendored copy carries is still present
#            upstream). On adoption we copy the cloud spec over the vendored one
#            and refresh SOURCE (cloud commit sha + new sha256).
#
#         B. Verify-and-register (downgrade) — when the specs diverge in a way
#            that would change or shrink the generated types. We do NOT replace
#            the vendored spec (that would break `generate_types!` consumers).
#            Instead we re-verify the vendored sha256 against SOURCE and update
#            the cloud provenance pointers (source-commit + a divergence note)
#            so the drift is recorded, not silently adopted.
#
# Usage:
#   scripts/sync-contract.sh                       # sync (auto A or B)
#   scripts/sync-contract.sh --check               # CI sha256 gate only
#   scripts/sync-contract.sh --cloud-repo <path>   # override cloud checkout

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
vendored_spec="$repo_root/crates/difflore-core/contracts/openapi-spec.json"
source_file="$repo_root/crates/difflore-core/contracts/SOURCE"

# Cloud spec defaults to a sibling checkout (the two repos live under a shared
# parent directory, per the blueprint).
cloud_repo_default="$(cd "$repo_root/.." && pwd)/difflore-cloud"
cloud_spec_rel="src/contracts/openapi/api.json"

# ── helpers ──────────────────────────────────────────────────────────────────
sha256_of() {
  # Portable sha256: prefer shasum (macOS), fall back to sha256sum (Linux).
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

source_field() {
  # Extract "key: value" from SOURCE, trimming leading whitespace from value.
  grep -E "^$1:" "$source_file" | head -n1 | sed -E "s/^$1:[[:space:]]*//"
}

# Rewrite SOURCE for branch A (adopted): bump source-commit + spec-sha256 and
# drop any stale divergence note. Written fresh so the file stays canonical.
refresh_source_adopted() {
  local commit="$1" sha="$2"
  cat > "$source_file" <<EOF
# Provenance for crates/difflore-core/contracts/openapi-spec.json
#
# The spec is vendored from the difflore-cloud repository's contract export
# (cloud: \`pnpm contract:export\` writes src/contracts/openapi/*.json).
# Sync via scripts/sync-contract.sh; do not hand-edit openapi-spec.json
# without re-running it (the --check gate enforces the sha256 below).

source-repo:   difflore-cloud
source-path:   $cloud_spec_rel
source-commit: $commit

# sha256 of the vendored openapi-spec.json currently checked in here.
spec-sha256:   $sha

# Last sync adopted the cloud spec directly (structurally compatible, no
# generated-type regression). In sync with cloud at source-commit above.
EOF
}

# Rewrite SOURCE for branch B (downgrade): keep the verified vendored sha256,
# but register the divergent cloud commit + a note describing the gap.
register_cloud_divergence() {
  local cloud_commit="$1" cloud_path="$2" vendored_only="$3" vendored_sha="$4" cloud_sha="$5"
  cat > "$source_file" <<EOF
# Provenance for crates/difflore-core/contracts/openapi-spec.json
#
# The spec is vendored from the difflore-cloud repository's contract export
# (cloud: \`pnpm contract:export\` writes src/contracts/openapi/*.json).
# Sync via scripts/sync-contract.sh; do not hand-edit openapi-spec.json
# without re-running it (the --check gate enforces the sha256 below).

source-repo:   difflore-cloud
source-path:   $cloud_path
source-commit: $cloud_commit

# sha256 of the vendored openapi-spec.json currently checked in here.
spec-sha256:   $vendored_sha

# sha256 of the divergent cloud export at source-commit (NOT vendored — see
# DIVERGENCE below). Lets a future sync detect when cloud has moved.
cloud-spec-sha256: $cloud_sha

# DIVERGENCE: the cloud export at source-commit is structurally compatible
# (identical top-level keys, path set, and component-schema names) but its
# generated-type surface differs from the vendored copy by $vendored_only
# field line(s) that exist only in the vendored spec. Adopting the cloud spec
# directly would SHRINK the types produced by \`generate_types!\`, so
# sync-contract.sh deliberately did NOT replace the vendored spec. The
# vendored sha256 above was re-verified against this file before registering
# the cloud commit. Convergence is tracked for the C1/C5 cloud batches
# (export diff-empty gate) — see REORG-BLUEPRINT.md section 5.
EOF
}

# ── arg parsing ──────────────────────────────────────────────────────────────
mode="sync"
cloud_repo="$cloud_repo_default"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --check) mode="check"; shift ;;
    --cloud-repo) cloud_repo="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,44p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "sync-contract: unknown argument: $1" >&2; exit 2 ;;
  esac
done

# ── --check mode: sha256 gate only ───────────────────────────────────────────
if [ "$mode" = "check" ]; then
  if [ ! -f "$vendored_spec" ]; then
    echo "sync-contract: vendored spec missing: $vendored_spec" >&2
    exit 1
  fi
  recorded="$(source_field 'spec-sha256')"
  actual="$(sha256_of "$vendored_spec")"
  if [ "$recorded" != "$actual" ]; then
    echo "sync-contract: SHA256 MISMATCH — vendored spec changed without a SOURCE refresh" >&2
    echo "  SOURCE spec-sha256: $recorded" >&2
    echo "  actual sha256:      $actual" >&2
    echo "  Run scripts/sync-contract.sh to re-vendor + refresh SOURCE." >&2
    exit 1
  fi
  echo "sync-contract: OK (vendored spec matches SOURCE spec-sha256)"
  exit 0
fi

# ── sync mode ────────────────────────────────────────────────────────────────
cloud_spec="$cloud_repo/$cloud_spec_rel"
if [ ! -f "$cloud_spec" ]; then
  echo "sync-contract: cloud spec not found: $cloud_spec" >&2
  echo "  Point --cloud-repo at a difflore-cloud checkout that has run" >&2
  echo "  'pnpm contract:export', or run that export first." >&2
  exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "sync-contract: jq is required for structural comparison" >&2
  exit 1
fi

cloud_commit="$(git -C "$cloud_repo" rev-parse HEAD 2>/dev/null || echo "unknown")"
vendored_sha="$(sha256_of "$vendored_spec")"
cloud_sha="$(sha256_of "$cloud_spec")"

# Structural comparison on key-sorted JSON across three axes: top-level keys,
# the set of paths, and the set of component-schema names.
struct_keys() { jq -S 'keys' "$1"; }
struct_paths() { jq -S '.paths | keys' "$1"; }
struct_schemas() { jq -S '.components.schemas | (keys? // [])' "$1"; }

structurally_compatible=1
for axis in keys paths schemas; do
  if ! diff -q <(struct_"$axis" "$vendored_spec") <(struct_"$axis" "$cloud_spec") >/dev/null; then
    structurally_compatible=0
  fi
done

# Regression guard: would adopting the cloud spec DROP any property/required
# field the vendored copy currently carries? generate_types! turns those into
# struct fields, so a drop is a breaking change to generated.rs consumers. We
# detect it by diffing the fully-normalized specs and counting lines that exist
# only on the vendored side ('<').
normalized_diff="$(diff <(jq -S . "$vendored_spec") <(jq -S . "$cloud_spec") || true)"
vendored_only_lines="$(printf '%s\n' "$normalized_diff" | grep -c '^<' || true)"

if [ "$structurally_compatible" = "1" ] && [ -z "$normalized_diff" ]; then
  echo "sync-contract: vendored spec already matches cloud (normalized identical)."
  echo "  cloud commit: $cloud_commit"
  echo "  sha256:       $vendored_sha"
  # Still refresh SOURCE so source-commit tracks the latest matching cloud HEAD.
  refresh_source_adopted "$cloud_commit" "$vendored_sha"
  exit 0
fi

if [ "$structurally_compatible" = "1" ] && [ "$vendored_only_lines" -eq 0 ]; then
  # ── Branch A: direct adoption ──────────────────────────────────────────────
  echo "sync-contract: direct adoption — cloud spec is structurally compatible"
  echo "  and adds fields without dropping any. Copying + refreshing SOURCE."
  cp "$cloud_spec" "$vendored_spec"
  new_sha="$(sha256_of "$vendored_spec")"
  refresh_source_adopted "$cloud_commit" "$new_sha"
  echo "sync-contract: adopted. NOTE: re-run cargo check/clippy/test —"
  echo "  generate_types! now produces the new fields."
  exit 0
fi

# ── Branch B: verify-and-register (downgrade) ────────────────────────────────
echo "sync-contract: DOWNGRADE — cloud spec diverges in a way that would" >&2
echo "  change/shrink generated types (vendored-only structural lines: $vendored_only_lines)." >&2
echo "  Not replacing the vendored spec; verifying sha256 + registering cloud" >&2
echo "  provenance instead. See SOURCE 'DIVERGENCE' note." >&2

recorded="$(source_field 'spec-sha256')"
if [ "$recorded" != "$vendored_sha" ]; then
  echo "sync-contract: SHA256 MISMATCH on the vendored spec itself —" >&2
  echo "  the checked-in spec does not match SOURCE spec-sha256." >&2
  echo "  SOURCE: $recorded  actual: $vendored_sha" >&2
  echo "  Refusing to register cloud provenance over an unverified spec." >&2
  exit 1
fi

register_cloud_divergence "$cloud_commit" "$cloud_spec_rel" "$vendored_only_lines" "$vendored_sha" "$cloud_sha"
echo "sync-contract: registered cloud provenance (commit $cloud_commit)."
echo "  Vendored spec verified against SOURCE; not replaced."
exit 0
