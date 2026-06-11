#!/usr/bin/env bash
# Local green-gate: run the same checks CI enforces, in the same
# order, with one command. Mirrors `.github/workflows/ci.yml`
# (`stable` job steps + the msrv job's all-targets build); if CI
# gains or changes a step, update this script in the same change.
#
# Usage:
#     ./scripts/verify.sh
#
# Requires the LLVM 18 dev libs for `relon-codegen-llvm` (see
# `crates/relon-codegen-llvm/Cargo.toml` for the local pin).

set -euo pipefail
cd "$(dirname "$0")/.."

run() {
    echo
    echo "==> $*"
    "$@"
}

run cargo fmt --all -- --check
run cargo build --workspace --all-targets
run cargo clippy --workspace --all-targets -- -D warnings
run cargo test --workspace
run cargo run -q -p relon-fmt -- --check \
    fixtures/*.relon fixtures/modules/*.relon fixtures/errors/*.relon \
    examples/*.relon

echo
echo "verify: all gates green"
