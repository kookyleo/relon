#!/usr/bin/env bash
# v6-λ-machine (2026-05-19): bring the host into "quiescent" state before
# the Relon vs LuaJIT bench round. Mirrors `docs/internal/archive/relon-vs-luajit-
# rigorous-plan.md` §6.
#
# Usage:
#     ./scripts/bench_quiescence.sh        # interactive; needs sudo
#     ./scripts/bench_quiescence.sh --check # read-only verification, no sudo
#
# The script is NOT executed by the bench agent — privileged ops live here
# for the human to invoke once per bench round. The Rust-side companion
# (`crates/relon-bench/src/quiescence.rs`) does a read-only `verify_quiescence`
# check at bench startup and refuses to run if the machine isn't quiescent.
#
# Exit codes:
#     0 — machine is quiescent and ready to bench
#     1 — generic failure (missing dependency, sysfs read error, etc.)
#     2 — machine is too noisy (context-switches/sec/core > 100 in 5s sample)
#     3 — a required sysfs knob couldn't be set
#
# License: Apache-2.0

set -euo pipefail

CHECK_ONLY=0
if [[ "${1:-}" == "--check" ]]; then
    CHECK_ONLY=1
fi

red()    { printf '\033[0;31m%s\033[0m\n' "$*"; }
green()  { printf '\033[0;32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[0;33m%s\033[0m\n' "$*"; }
bold()   { printf '\033[1m%s\033[0m\n' "$*"; }

NCPU=$(nproc)
BENCH_CPUS="${BENCH_CPUS:-4-7}"

bold "=== Relon bench quiescence (CPUs: $NCPU; bench-pinned cores: $BENCH_CPUS) ==="

# ---- 1. CPU governor: performance --------------------------------------
bold "[1/5] CPU governor"
ANY_GOV_NON_PERF=0
for gov_file in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    gov=$(cat "$gov_file" 2>/dev/null || echo "missing")
    if [[ "$gov" != "performance" ]]; then
        ANY_GOV_NON_PERF=1
        yellow "  $gov_file = $gov (expected: performance)"
    fi
done
if [[ "$ANY_GOV_NON_PERF" == "1" ]]; then
    if [[ "$CHECK_ONLY" == "1" ]]; then
        red "  governor not performance (read-only mode; not fixing)"
    else
        yellow "  Setting governor to performance (sudo required)..."
        if ! sudo cpupower frequency-set -g performance > /dev/null 2>&1; then
            # Fallback: write directly to sysfs
            for gov_file in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
                echo performance | sudo tee "$gov_file" > /dev/null || true
            done
        fi
        # Re-verify
        STILL_BAD=0
        for gov_file in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
            gov=$(cat "$gov_file" 2>/dev/null || echo "missing")
            [[ "$gov" != "performance" ]] && STILL_BAD=1
        done
        if [[ "$STILL_BAD" == "1" ]]; then
            red "  Could not set governor to performance on every CPU."
            red "  Proceed with caution; bench results may have freq-drift noise."
        else
            green "  All CPUs now at performance governor."
        fi
    fi
else
    green "  All CPUs already at performance governor."
fi

# ---- 2. Turbo boost off -----------------------------------------------
bold "[2/5] Turbo boost"
NO_TURBO_PATH=/sys/devices/system/cpu/intel_pstate/no_turbo
if [[ -e "$NO_TURBO_PATH" ]]; then
    NT=$(cat "$NO_TURBO_PATH")
    if [[ "$NT" == "1" ]]; then
        green "  intel_pstate/no_turbo = 1 (turbo disabled)"
    else
        if [[ "$CHECK_ONLY" == "1" ]]; then
            red "  no_turbo = $NT (read-only mode; not fixing)"
        else
            yellow "  Disabling turbo (sudo required)..."
            echo 1 | sudo tee "$NO_TURBO_PATH" > /dev/null
            NT=$(cat "$NO_TURBO_PATH")
            if [[ "$NT" == "1" ]]; then
                green "  no_turbo = 1"
            else
                red "  Could not disable turbo; bench may see freq-drift outliers."
            fi
        fi
    fi
else
    BOOST_PATH=/sys/devices/system/cpu/cpufreq/boost
    if [[ -e "$BOOST_PATH" ]]; then
        B=$(cat "$BOOST_PATH")
        if [[ "$B" == "0" ]]; then
            green "  cpufreq/boost = 0 (AMD turbo disabled)"
        else
            if [[ "$CHECK_ONLY" == "1" ]]; then
                red "  cpufreq/boost = $B (read-only mode; not fixing)"
            else
                yellow "  Disabling boost (sudo required)..."
                echo 0 | sudo tee "$BOOST_PATH" > /dev/null
            fi
        fi
    else
        yellow "  Neither intel_pstate/no_turbo nor cpufreq/boost found; assuming no turbo knob."
    fi
fi

# ---- 3. Thermal state report ------------------------------------------
bold "[3/5] Thermal state"
for tz in /sys/class/thermal/thermal_zone*/temp; do
    [[ -e "$tz" ]] || continue
    zone=$(dirname "$tz")
    type=$(cat "$zone/type" 2>/dev/null || echo "?")
    millicel=$(cat "$tz" 2>/dev/null || echo "0")
    cel=$((millicel / 1000))
    printf "  %-32s %3d C  (%s)\n" "$(basename "$zone")" "$cel" "$type"
done

# ---- 4. Bench-CPU pinning hint ----------------------------------------
bold "[4/5] Bench-core pinning"
echo "  Suggested invocation:"
echo "      taskset -c $BENCH_CPUS cargo bench -p relon-bench --bench cmp_lua"
echo "  Bench CPUs ($BENCH_CPUS) reserved for the bench; ideally isolcpus= them at boot."

# ---- 5. Baseline noise (5s perf stat) ---------------------------------
bold "[5/5] Baseline noise (5s perf stat)"
if ! command -v perf > /dev/null 2>&1; then
    yellow "  perf not installed; skipping noise check."
    yellow "  Install with: sudo apt install linux-tools-common linux-tools-\$(uname -r)"
else
    TMP=$(mktemp)
    if perf stat -a -e context-switches,cache-misses -- sleep 5 2> "$TMP"; then
        : # ok
    fi
    CS=$(grep 'context-switches' "$TMP" | awk '{ gsub(",", "", $1); print $1 }' || echo "0")
    CM=$(grep 'cache-misses' "$TMP" | awk '{ gsub(",", "", $1); print $1 }' || echo "0")
    rm -f "$TMP"
    if [[ -z "$CS" ]]; then CS=0; fi
    if [[ -z "$CM" ]]; then CM=0; fi
    CS_PER_CORE_PER_SEC=$(( CS / NCPU / 5 ))
    echo "  context-switches: $CS over 5s ($CS_PER_CORE_PER_SEC / core / sec)"
    echo "  cache-misses:     $CM over 5s"
    if (( CS_PER_CORE_PER_SEC > 100 )); then
        red "  ABORT: context-switches/sec/core ($CS_PER_CORE_PER_SEC) > 100. Machine too noisy."
        red "  Close background apps (Slack/browser/IDE/Docker) and re-run."
        exit 2
    fi
    green "  Noise within budget."
fi

green "=== Machine appears quiescent. Ready to bench. ==="
echo
echo "Next steps:"
echo "  taskset -c $BENCH_CPUS cargo bench -p relon-bench --bench cmp_lua"
echo "  cargo run --release -p relon-bench --bin bench_stats -- \\"
echo "      target/criterion/cmp_lua"
