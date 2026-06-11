#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

allow_dirty=()
if [[ "${RELON_PACKAGE_ALLOW_DIRTY:-}" == "1" ]]; then
  allow_dirty=(--allow-dirty)
fi

# Keep this list aligned with crates that set `publish = false`.
# The first public release still package-checks Tier 3 crates when they
# are publishable; this only skips private workspace helpers.
excluded=(
  --exclude relon-bench
  --exclude relon-rs-demo
  --exclude relon-test-harness
  --exclude relon-wasm-bindings
)

cargo publish \
  --dry-run \
  --workspace \
  --locked \
  "${allow_dirty[@]}" \
  "${excluded[@]}"

echo "package-dry-run: all publishable workspace crates passed cargo publish --dry-run"
