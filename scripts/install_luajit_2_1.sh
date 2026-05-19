#!/usr/bin/env bash
# v6-λ-1 (2026-05-19): install LuaJIT 2.1-stable to a user-writable prefix.
# Non-interactive, no sudo required. The bench crate uses `mlua` with the
# `vendored` feature by default (which bundles LuaJIT itself), so this script
# is needed only if you want to switch to the system / shared LuaJIT install
# for a future system-vs-vendored boundary comparison.
#
# Usage:
#     ./scripts/install_luajit_2_1.sh
#
# What it does:
#     1. clones LuaJIT into /tmp/LuaJIT-src
#     2. checks out the long-term `v2.1` stable branch
#     3. builds with `make CCDEBUG=-g`
#     4. installs to /tmp/luajit-2.1/{bin,lib,include}
#     5. prints `luajit -v` so the version is captured
#     6. prints PKG_CONFIG_PATH export hint for mlua's `system` feature
#
# License: Apache-2.0

set -euo pipefail

PREFIX="${LUAJIT_PREFIX:-/tmp/luajit-2.1}"
SRC_DIR="${LUAJIT_SRC_DIR:-/tmp/LuaJIT-src}"
REPO="${LUAJIT_REPO:-https://github.com/LuaJIT/LuaJIT.git}"
BRANCH="${LUAJIT_BRANCH:-v2.1}"

bold()  { printf '\033[1m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
red()   { printf '\033[0;31m%s\033[0m\n' "$*"; }

bold "=== Installing LuaJIT (branch: $BRANCH) → $PREFIX ==="

# ---- prereqs ----------------------------------------------------------
for tool in git make cc; do
    if ! command -v "$tool" > /dev/null 2>&1; then
        red "Missing dependency: $tool"
        echo "  Install with: sudo apt install build-essential git"
        exit 1
    fi
done

# ---- 1. clone or update ------------------------------------------------
if [[ -d "$SRC_DIR/.git" ]]; then
    bold "[1/5] Updating $SRC_DIR"
    git -C "$SRC_DIR" fetch --tags origin "$BRANCH"
    git -C "$SRC_DIR" checkout "$BRANCH"
    git -C "$SRC_DIR" reset --hard "origin/$BRANCH"
else
    bold "[1/5] Cloning $REPO into $SRC_DIR"
    rm -rf "$SRC_DIR"
    git clone --branch "$BRANCH" "$REPO" "$SRC_DIR"
fi

HEAD_SHA=$(git -C "$SRC_DIR" rev-parse --short HEAD)
HEAD_DATE=$(git -C "$SRC_DIR" log -1 --format=%cd --date=short)
green "  LuaJIT source at $HEAD_SHA ($HEAD_DATE)"

# ---- 2. clean build env ------------------------------------------------
bold "[2/5] Cleaning prior build artifacts"
make -C "$SRC_DIR" clean > /dev/null 2>&1 || true
rm -rf "$PREFIX"

# ---- 3. build ----------------------------------------------------------
bold "[3/5] Building (this takes ~30s)"
# `CCDEBUG=-g` keeps debug symbols in the shared lib so perf flamegraphs
# annotate Lua frames. PREFIX is baked into the binary so `luajit` finds
# its bundled modules.
make -C "$SRC_DIR" CCDEBUG=-g PREFIX="$PREFIX" -j"$(nproc)"

# ---- 4. install ---------------------------------------------------------
bold "[4/5] Installing into $PREFIX"
make -C "$SRC_DIR" install PREFIX="$PREFIX"

# ---- 5. version + pkg-config hint --------------------------------------
bold "[5/5] Reporting installed version"
if [[ -x "$PREFIX/bin/luajit" ]]; then
    "$PREFIX/bin/luajit" -v
else
    red "luajit binary missing at $PREFIX/bin/luajit"
    exit 1
fi

PC_PATH="$PREFIX/lib/pkgconfig"
if [[ -d "$PC_PATH" ]]; then
    green "pkg-config files at: $PC_PATH"
    echo
    echo "To use this LuaJIT with mlua's `system` feature instead of `vendored`:"
    echo "  export PKG_CONFIG_PATH=\"$PC_PATH:\${PKG_CONFIG_PATH:-}\""
    echo "  export LD_LIBRARY_PATH=\"$PREFIX/lib:\${LD_LIBRARY_PATH:-}\""
    echo "  # Then in Cargo.toml change mlua feature `vendored` → no `vendored`."
else
    red "pkg-config dir missing; mlua system mode won't work."
fi

green "=== LuaJIT install complete. ==="
