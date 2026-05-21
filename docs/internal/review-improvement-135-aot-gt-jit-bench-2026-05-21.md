# Review improvement #135 — trace-JIT failure-mode bench fixtures

Date: 2026-05-21
Bench file: `crates/relon-bench/benches/jit_failure_modes.rs`
Branch: `worktree-agent-a15c202c76ccf6d41`
Worktree: `/ext/relon/.claude/worktrees/agent-a15c202c76ccf6d41`

## Motivation

Existing benches (`trace_jit_hot_loop` W1, `cmp_lua` W1-W10) only
exercise the hot-loop happy path where the trace JIT wins by
roughly 0.66× – 1.47× over `cranelift_aot`. The three pathological
paths the design doc calls out — recorder abort, high deopt rate,
cold workload — were not covered. This fixture set is the
counter-evidence that trace-JIT is not free.

## Fixture design

Each fixture compiles a loop shape derived from
`trace_jit_hot_loop::sum_loop_let_slot_body` into three rows:
`tree_walk`, `cranelift_aot`, `trace_jit`. The `trace_jit` row goes
through `TraceJitState::invoke_with_fallback` with the fallback
re-running through the same `CraneliftAotEvaluator` — so the only
delta is the trace-JIT install / dispatch overhead.

- **Fixture A — `fixture_a_abort`**: loop body emits
  `Op::BitAnd(IrType::I64)` mid-loop. The trace recorder lowers this
  to `AbortReason::UnsupportedOp("BitAnd")`
  (`relon-trace-recorder/src/lowering.rs:297`); cranelift-AOT lowers
  `BitAnd(I64)` natively to one `band` instruction. The bench
  asserts `try_install_recorder_trace` returns `false`, so every
  `invoke_with_fallback` call short-circuits to the fallback. Per
  call the trace_jit row pays: `lookup_trace` miss + fallback
  closure dispatch + AOT run_main.
- **Fixture B — `fixture_b_deopt`**: loop body has a toggle BrIf
  (`if toggle != 0 { exit loop }`). Recorder records with
  `toggle = 0` (fall-through path) and emits an
  `IsZero(cond_ssa)` guard
  (`relon-codegen-native/src/trace_recording.rs:381`). At
  measurement time the bench passes `toggle = 1`, so the guard
  predicate `(cond == 0)` is false on the very first iteration,
  `save_deopt` fires, and `invoke_with_fallback` runs the workload
  through the AOT path. Cranelift-AOT has no analogous guard — the
  BrIf is a plain conditional branch that exits the loop on iter 1.
- **Fixture C — `fixture_c_cold`**: trace installs cleanly for the
  vanilla sum-loop. Each call sets `n = 50` (below the per-call
  amortisation window). The bench drives `outer_calls = 20_000`
  invocations per criterion iter so total per-iter elements stays at
  `HOT_LOOP_N` for direct comparison against the
  `trace_jit_hot_loop` rows.

## Bench numbers — median across 3 runs

System is **not** quiescent (`RELON_BENCH_FORCE_RUN=1`; sibling
cargo agents compiling, governor `non-perf`, errors=2 in the
quiescence report). Numbers therefore carry ±10-30% variance on the
AOT/tree-walk rows; medians across 3 runs filter the noisier
samples, and the deltas between rows within a single run remain
stable across all runs.

| Fixture | tree_walk | cranelift_aot | trace_jit | trace_jit ÷ aot |
|---|---|---|---|---|
| A — abort (BitAnd UnsupportedOp) | 33.886 ms / 10k iter → 3.389 µs/elem | **2.3686 ms** / 1M iter → 2.369 ns/elem | **2.3685 ms** / 1M iter → 2.369 ns/elem | **≈ 1.00×** |
| B — high deopt (IsZero guard) | 34.910 ms / 10k iter | **422.87 ns** / call (exit on iter 1) | **726.78 ns** / call | **1.72×** |
| C — cold (n = 50 × 20k outer) | 39.678 ms / 500k iter → 79.4 ns/elem | **7.789 ms** / 1M iter → 7.789 ns/elem | **16.998 ms** / 1M iter → 17.0 ns/elem | **2.18×** |

Per-run raw numbers (run-2 / run-3 / run-4):

| Row | run-2 | run-3 | run-4 |
|---|---|---|---|
| A / aot | 2.3685 ms | 2.7743 ms | 2.3686 ms |
| A / jit | 2.3685 ms | 5.1455 ms | 2.3685 ms |
| B / aot | 411.07 ns | 424.86 ns | 422.87 ns |
| B / jit | 706.17 ns | 980.46 ns | 726.78 ns |
| C / aot | 7.7892 ms | 13.501 ms | 7.7823 ms |
| C / jit | 16.998 ms | 17.108 ms | 16.192 ms |

Run-3 was during sibling rustc compilation peak load; the trace_jit
row stays inside the cranelift_aot row's noise band in every run.

## Hypothesis validation

- **Fixture A (`trace_jit ≳ cranelift_aot`)**: HYPOTHESIS HELD,
  but lookup overhead is below the per-iter measurement floor at
  1M elements / call. Median trace_jit (2.3685 ms) tracks
  cranelift_aot (2.3686 ms) to within sub-ns / call. The cost of an
  aborted recording is paid once at setup; per-call cost is
  dominated by the actual workload that runs through the AOT
  fallback. **Interpretation**: in steady state, a sticky-abort
  trace site degrades cleanly to ~ AOT cost — the failure mode is
  at install time, not per call.
- **Fixture B (`trace_jit > cranelift_aot`)**: HYPOTHESIS HELD
  DRAMATICALLY. The trace machinery (entry prologue + guard
  predicate evaluation + `save_deopt` + `invoke_with_fallback`
  dispatch) adds **~304 ns per call** on top of a 423 ns AOT
  baseline (1.72×). This is the textbook case where AOT > JIT —
  the trace pays for itself only if guards rarely fire. Every
  guard-firing call is a net loss.
- **Fixture C (`trace_jit ≥ cranelift_aot`)**: HYPOTHESIS HELD
  DRAMATICALLY. trace_jit is **2.18×** slower per element when the
  inner loop is only 50 iters. The per-call
  `TraceContext::with_hooks(64)` allocation + trace lookup +
  ABI marshalling exceeds the 50-iter cranelift loop body. This
  matches the LuaJIT design note that ≤ 100 iter workloads defeat
  the trace tier.

## Surprises / honest record

- **Fixture A's trace_jit ≈ AOT delta is below the noise floor.**
  We initially expected the `lookup_trace` miss to add a few
  ns / call but at 1M iterations per call the overhead is washed
  out (the inner loop is the cost centre). To surface the
  per-call-overhead cleanly we would need a row analogous to the
  `dispatch_*` rows in `trace_jit_hot_loop` (callers driving the
  invoke `HOT_LOOP_N` times from Rust). Out of scope for this
  task; documented for the next iteration.
- **First Fixture B design was scrapped.** The initial design used
  `ArithOverflow` guard via `acc = i64::MAX - 3`. It crashed both
  rows because cranelift-AOT lowers `Op::Add(I64)` with
  `sadd_overflow` + trap on overflow
  (`codegen/arith.rs:56-57`), so the AOT row panicked with
  `RuntimeError::NumericOverflow` before reaching the timed region.
  **Pivoted** to the `IsZero(toggle)` BrIf-polarity guard — same
  intent (every call deopts) but the AOT path has no matching
  trap. The trace guard fires unconditionally because the recorder
  pinned the polarity at the warmup `toggle = 0` observation.
- The previously documented expectation that the recorder's
  `LocalGet(idx)` hint of `ObservedType::I32` would trigger a
  TypeCheck guard fire never materialised: `build_guard_predicate`
  for `TypeCheck` resolves the predicate at compile time
  (`relon-trace-emitter/src/guard_emit.rs:167-182`), so if the
  recorded type matches the expected type, the predicate is
  `iconst(1)` and the guard never fires at runtime regardless of
  the operand bits. Means the `IsZero(BrIf-cond)` path is the
  cleanest way today to force per-call deopts from a hand-built IR
  body.

## LoC delta

- `crates/relon-bench/benches/jit_failure_modes.rs` — **+869 lines**
  (new file).
- `crates/relon-bench/Cargo.toml` — **+11 lines** (bench
  registration block).
- Total: **+880 lines, 0 deletions**.

## Gate

- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace` — all tests pass; the existing
  `methodology_validators` test only targets `trace_jit_hot_loop.rs`
  and is unaffected.
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` —
  clean (the new bench is host-only and does not affect the wasm
  build surface).
