#!/usr/bin/env bash
# CPU flamegraph for the relon-bench profile_alloc workloads.
#
# Runs `cargo flamegraph` against each profile_alloc workload mode and
# writes the resulting SVG into target/flamegraph/. The bench itself
# already lives in main; this script just wires up the perf sampling.
#
# Usage:
#     ./scripts/perf-flamegraph.sh                          # default: comprehension
#     ./scripts/perf-flamegraph.sh comprehension-pooled     # single mode
#     ./scripts/perf-flamegraph.sh all                      # all 4 modes
#     ./scripts/perf-flamegraph.sh --help
#
# Dependencies:
#     - cargo flamegraph (cargo install flamegraph)
#     - linux-tools-common / perf
#     - kernel.perf_event_paranoid <= 1 (the script detects and explains
#       how to lower it; it does NOT call sudo itself)
#
# Output:
#     target/flamegraph/<mode>.svg
#     Open in a browser for click-to-zoom and search.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

ALL_MODES=(simple simple-pooled comprehension comprehension-pooled)

print_help() {
    sed -n '2,/^$/{s/^# \{0,1\}//;p}' "$0" | head -n 22
}

case "${1:-}" in
    -h|--help)
        print_help
        exit 0
        ;;
esac

# Resolve which modes to run.
if [[ $# -eq 0 ]]; then
    MODES=(comprehension)
elif [[ "$1" == "all" ]]; then
    MODES=("${ALL_MODES[@]}")
else
    MODES=("$@")
fi

# Validate mode names.
for m in "${MODES[@]}"; do
    found=0
    for valid in "${ALL_MODES[@]}"; do
        [[ "$m" == "$valid" ]] && { found=1; break; }
    done
    if [[ $found -eq 0 ]]; then
        echo "unknown mode: $m" >&2
        echo "valid: ${ALL_MODES[*]}" >&2
        exit 2
    fi
done

# Ensure cargo flamegraph is available.
if ! command -v cargo-flamegraph >/dev/null 2>&1 && ! cargo flamegraph --version >/dev/null 2>&1; then
    cat >&2 <<'EOF'
cargo flamegraph is not installed. Install once:
    cargo install flamegraph
(also make sure linux-tools-common / perf is installed)
EOF
    exit 1
fi

# Guard the kernel.perf_event_paranoid setting. Do NOT call sudo from
# here -- print the command and let the operator decide.
PARANOID_FILE=/proc/sys/kernel/perf_event_paranoid
if [[ ! -r "$PARANOID_FILE" ]]; then
    echo "cannot read $PARANOID_FILE; skipping permission check (not Linux?)" >&2
else
    CUR=$(cat "$PARANOID_FILE")
    if (( CUR > 1 )); then
        cat >&2 <<EOF
kernel.perf_event_paranoid = $CUR -- this blocks perf sampling (need <= 1).

Run this in another terminal (one-shot, reverts on reboot):
    sudo sysctl kernel.perf_event_paranoid=1

Or persist it:
    echo 'kernel.perf_event_paranoid = 1' | sudo tee /etc/sysctl.d/99-perf.conf
    sudo sysctl --system

Then re-run this script.
EOF
        exit 1
    fi
fi

OUT_DIR="$REPO_ROOT/target/flamegraph"
mkdir -p "$OUT_DIR"

# Force debug line tables + frame pointers so perf gets readable stacks.
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
export RUSTFLAGS="${RUSTFLAGS:-} -Cstrip=none -Cforce-frame-pointers=yes"

echo "==> building release profile_alloc (with debuginfo + frame pointers)"
cargo build --release -p relon-bench --bin profile_alloc

# Per-mode iteration scale, picked so each run lasts ~5 seconds of wall
# time at @997 Hz sampling -> ~5000 samples per flamegraph.
# Base counts: SIMPLE_ITERATIONS=1000, COMPREHENSION_ITERATIONS=100.
# Approx wall time per iter (post-P2 main, mid-range desktop):
#   simple one-shot       ~ 30 us   -> need ~170 000 iters -> scale 170
#   simple-pooled         ~ 10 us   -> need ~500 000 iters -> scale 500
#   comprehension         ~ 2.5 ms  -> need ~  2 000 iters -> scale  20
#   comprehension-pooled  ~ 1.5 ms  -> need ~  3 500 iters -> scale  35
declare -A SCALE_FOR=(
    [simple]=200
    [simple-pooled]=500
    [comprehension]=20
    [comprehension-pooled]=35
)

for mode in "${MODES[@]}"; do
    out="$OUT_DIR/$mode.svg"
    scale="${SCALE_FOR[$mode]}"
    echo
    echo "==> [$mode] flamegraph -> $out  (PROFILE_ALLOC_SCALE=$scale)"
    # --freq 997: sampling rate (default 99 is too coarse; 997 is prime
    # to avoid aliasing with periodic workload phases)
    PROFILE_ALLOC_SCALE="$scale" cargo flamegraph \
        --release \
        -p relon-bench \
        --bin profile_alloc \
        --output "$out" \
        --freq 997 \
        -- "$mode"
done

echo
echo "done. SVG outputs:"
for mode in "${MODES[@]}"; do
    echo "  $OUT_DIR/$mode.svg"
done
echo
echo "open any SVG in a browser to drill down / search."
