# Relon vs LuaJIT — final report (v6-lambda-2/3/5)

**Status**：IN-FLIGHT（incrementally written as each W lands；final verdict appended at end）
**Base**：`99646ac feat(bench): v6-lambda-machine quiescence + v6-lambda-1 LuaJIT install`
**Methodology**：v6-λ-0 hardened harness (6 traps mitigated) + λ-机器 quiescence gate + λ-1 LuaJIT (vendored 2.1) install
**Bench**：`crates/relon-bench/benches/cmp_lua.rs` — single criterion bench with 12 paired workloads (Relon tree-walker + LuaJIT) + W11 cold-start sub-group + W12 p99 tail
**Host**：dev box (non-quiescent — governor=schedutil per `λ-机器` report). `RELON_BENCH_FORCE_RUN=1` was set; all numbers below carry a ±10% noise envelope per λ-0 reproducibility data.

---

## 0. Executive summary

(Filled at end of report after all 12 W rows + 5-dim判定 are written.)

---

## 1. Methodology recap

Per `docs/internal/relon-vs-luajit-rigorous-plan.md` §2, the bench harness
hardens the 6 known measurement traps:

| Trap | Mitigation |
|---|---|
| A — compiler elimination | every measurement closure passes inputs + outputs through `criterion::black_box(..)` — see `cmp_lua.rs` per-closure |
| B — warm-up vs steady-state | `timed_with_warmup()` helper runs `WARMUP_ITERS = 10_000` (or wall-clock cap 200ms) before `Instant::now()` |
| C — caller-side overhead | each workload drives `N_per_call` inner iters per closure call (W1: 10k, W7: fib(28) ≈ 317k tree-walks, etc.); criterion's `Throughput::Elements(N)` reports per-element cost |
| D — cache cold/hot | one full prefill `routine()` before the warmup loop pulls callee i-cache + arg memory into L1/L2 |
| E — GC vs no-GC | Relon today is `#[zero_alloc]` on the hot arithmetic rows; tree-walker rows that traverse Lists/Dicts allocate per call — tagged `#[per_iter_alloc]` honestly |
| F — distribution hiding | `sample_size = 100` (cmp_lua) / 200 (trace_jit_hot_loop) → `bench_stats` post-processor extracts p50/p90/p99/p99.9/max per row |

The Lua side is mlua/LuaJIT 2.1-vendored. Each Lua function is compiled
once outside the timed region; `Function::call(())` drives the hot loop.
The Lua **boundary calibration** is captured as `lua_boundary_calibrate`
in `trace_jit_hot_loop` (≈ 95 ns/call on the dev box, λ-1 measurement)
and is NOT subtracted from the cmp_lua numbers here; subtraction notes
are inline below where the boundary cost is a significant fraction of the
per-call time.

## 2. Per-workload results

The bench output below is captured into `target/criterion/v6_lambda_cmp_lua/`
and post-processed via:

```bash
cargo run --release -p relon-bench --bin bench_stats -- \
    target/criterion/v6_lambda_cmp_lua
```

Each row is one `(workload, backend)` cell. The `ratio` column is
`relon_p50 / lua_p50`; **lower is better for Relon**. ratio ≤ 2.0 → PASS
for the underlying dimension (per rigorous plan §5 decision matrix).

| Workload | Relon p50 (ns/elem) | LuaJIT p50 (ns/elem) | ratio | Notes |
|---|---|---|---|---|
| W1 int sum | (pending) | (pending) | — | (notes per W) |
| W2 f64 dot | — | — | — | — |
| W3 string concat | — | — | — | — |
| W4 string contains | — | — | — | — |
| W5 dict str-key | — | — | — | — |
| W6 dict num-key | — | — | — | — |
| W7 fib(28) | — | — | — | — |
| W8 poly callsite | — | — | — | — |
| W9 matrix transpose | — | — | — | — |
| W10 config eval | — | — | — | — |
| W11 cold start | — | — | — | — |
| W12 p99 tail | — | — | — | — |

(populated after the bench round finishes; see §6 for raw `bench_stats` output appended verbatim.)

## 3. Per-dimension PASS / FAIL

Decision matrix per rigorous-plan §5:

| Dimension | Workloads | PASS condition | Status |
|---|---|---|---|
| D1 hot loop | W1+W2+W7+W9 | ≥ 1 W ratio ≤ 2.0 | (pending) |
| D2 cold start | W11 | W11 ratio ≤ 2.0 | (pending) |
| D5 p99 tail | W12 | W12 p99 + p99.9 ratio ≤ 2.0 | (pending) |
| D7 string | W3+W4 | ≥ 1 W ratio ≤ 2.0 | (pending) |
| D8 hash table | W5+W6 | ≥ 1 W ratio ≤ 2.0 | (pending) |

Overall: 5/5 → PASS; 3-4/5 → ITERATE; <3/5 → HONEST FAIL.

## 4. Attribution

(Filled in per W with ratio > 2.0 once data lands.)

## 5. Reproduction

```bash
# 1. Quiescence setup (sudo required; one-time)
sudo ./scripts/bench_quiescence.sh

# 2. Build all crates + bench
cargo build --workspace --release

# 3. Run cmp_lua paired bench
taskset -c 4-7 cargo bench --bench cmp_lua

# 4. Post-process distributions
cargo run --release -p relon-bench --bin bench_stats -- \
    target/criterion/v6_lambda_cmp_lua

cargo run --release -p relon-bench --bin bench_stats -- \
    target/criterion/v6_lambda_cmp_lua_cold
```

Machine:
- (filled in by `verify_quiescence()` report stderr at bench start)

LuaJIT: vendored 2.1 (via mlua 0.10 `luajit` + `vendored` feature flag).

## 6. Raw `bench_stats` output

(Appended verbatim from `bench_stats` after the bench round completes.)

## 7. Carry-over

(Per-fail-dimension targeted fix plan appended once verdict lands.)
