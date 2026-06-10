#!/usr/bin/env bash
# Layer gate: structural lints for the cargo workspace.
#
#   1. Orphan-module vaccine — every first-level directory under
#      crates/*/src must be declared as a module in that crate's lib.rs or
#      main.rs. Directories that compile to nothing are dead weight at best
#      and silently-rotting code at worst (see the agent_cli incident).
#   2. Domain purity — crates/difflore-core/src/domain must not import the
#      cloud / store / infra / context layers. Domain is a leaf: it may use
#      `crate::error` and other domain modules only.
#
# Checked in with batch R1b; CI wiring lands in R4. Until then run manually:
#
#   scripts/layer-gate.sh

set -euo pipefail

cd "$(dirname "$0")/.."

fail=0

# ── Gate 1: orphan-module vaccine ────────────────────────────────────────
#
# Known exemptions (crate:module, one per line). Empty since R2 mounted the
# three orphan trees in difflore-cli; this list may only shrink.
exemptions=""

for crate_dir in crates/*/; do
    crate=$(basename "$crate_dir")
    src="${crate_dir}src"
    [ -d "$src" ] || continue

    roots=()
    [ -f "$src/lib.rs" ] && roots+=("$src/lib.rs")
    [ -f "$src/main.rs" ] && roots+=("$src/main.rs")
    if [ "${#roots[@]}" -eq 0 ]; then
        echo "layer-gate: $crate has no src/lib.rs or src/main.rs" >&2
        fail=1
        continue
    fi

    for dir in "$src"/*/; do
        [ -d "$dir" ] || continue
        mod=$(basename "$dir")
        # src/bin is a cargo convention (binary targets), never a module.
        [ "$mod" = "bin" ] && continue
        if printf '%s\n' "$exemptions" | grep -qx "${crate}:${mod}"; then
            echo "layer-gate: exempt orphan (R2 pending): ${crate}/src/${mod}/"
            continue
        fi
        if ! grep -Eq "^[[:space:]]*(pub(\((crate|super)\))?[[:space:]]+)?mod[[:space:]]+${mod};" "${roots[@]}"; then
            echo "layer-gate: ORPHAN MODULE ${crate}/src/${mod}/ — not declared in ${roots[*]}" >&2
            fail=1
        fi
    done
done

# ── Gate 2: domain purity ────────────────────────────────────────────────
domain_dir="crates/difflore-core/src/domain"
if violations=$(grep -rnE 'use crate::(cloud|store|infra|context)' "$domain_dir" --include='*.rs'); then
    echo "layer-gate: DOMAIN LAYER VIOLATION — domain/ must not import cloud/store/infra/context:" >&2
    printf '%s\n' "$violations" >&2
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo "layer-gate: FAILED" >&2
    exit 1
fi
echo "layer-gate: OK"
