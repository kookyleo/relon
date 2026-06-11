#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

./scripts/verify.sh

echo
echo "==> cargo test -p relon --examples"
cargo test -p relon --examples

if [ ! -d docs/node_modules ]; then
  (cd docs && npm ci)
fi

(cd docs && npm run docs:build)

echo "release-verify: all gates green"
