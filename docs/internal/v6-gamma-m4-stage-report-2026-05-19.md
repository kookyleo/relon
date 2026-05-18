# v6-γ M4 Stage Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Status: M4 delivered. M5 (full corpus 3-way differential + bench) is
  the only milestone remaining for v6-γ phase wrap.
Companion: `docs/internal/v6-gamma-integration-plan-2026-05-18.md`,
  `docs/internal/v6-gamma-m2m3-stage-report-2026-05-19.md`.

---

## 0. TL;DR

- M4 ships every one of the six in-scope items: real `__relon_jump_to_recorder`,
  recorder `record_guard` fix, host_hooks.save_deopt wire, deopt
  fallback path, three-way differential harness, plus the stage
  report. No item silently slipped.
- Gate green: 1693 passing tests (baseline 1654 + 39 new), `cargo
  build` / `cargo test --workspace` / `cargo clippy --workspace
  --all-targets -- -D warnings` / `cargo fmt --all -- --check` /
  `cargo build --target wasm32-unknown-unknown -p relon-wasm` all
  clean.
- 5 commits, focused per-concern (`fix → IR walker → real helper →
  abi wire → harness`).

## 1. What landed

### 1.1 fix(trace-recorder): record_guard called inside append path

`crates/relon-trace-recorder/src/recorder.rs` —
`RecorderState` now exposes a `next_external_pc` monotone counter
(overridable via `set_next_external_pc(pc)`) and routes every
`TraceOp::Guard(kind, payload)` append through a new
`append_guard_with_site` helper that **also** calls
`TraceBuffer::record_guard(GuardSite::new(trace_pc, external_pc,
kind))`. Both the explicit-guard path (`emit_guard`) and the
type-observation path (`maybe_emit_type_guard`) flow through this
helper.

Net effect: every `Guard` op in `buf.ops` is paired with exactly
one matching `GuardSite` in `buf.guards`. The emitter's per-pc
guard lookup (`HashMap<u32, &GuardSite>`) now resolves on every
arith / div / load trace, eliminating `EmitError::OrphanGuardOp`
on the recorder-driven happy path.

Public surface additions:
- `RecorderState::set_next_external_pc(u64)`
- `RecorderState::abort_reason() -> Option<AbortReason>`

7 new tests in `tests/orphan_guard_fixed.rs` pin the
op-pc / site-pc balance across `Add` / `Sub` / `Mul` / `Div` plus
the `external_pc` override.

### 1.2 feat(codegen-native): TraceRecordingEvaluator IR walker

New module `crates/relon-codegen-native/src/trace_recording.rs`.

A single-shot stack-machine interpreter for the Phase-1 hot
subset:
- Const ops: `ConstI32` / `ConstI64` / `ConstBool`.
- Arithmetic: `Add` / `Sub` / `Mul` / `Div` on `I64`.
- Comparisons: `Eq` / `Ne` / `Lt` / `Le` / `Gt` / `Ge` on `I64`.
- Locals: `LocalGet` / `LetGet` / `LetSet`.
- Terminator: `Return`.

The walker runs the IR for real (concrete `u64` cells stack-based)
while feeding every op into a borrowed `RecorderState`. The
`StackCell { value, ssa, ty }` triple lets the recorder receive
correctly-typed SSA inputs without re-deriving them from the
operand stack. Ops outside the envelope abort the recording
cleanly — the helper falls back, the counter saturates.

Public surface: `TraceRecordingEvaluator`, `RecordingOutcome`
(`Recorded { recorder, result }` / `Aborted { reason,
partial_result }`), `StackCell`.

8 unit tests cover const / arith / cmp / local / let / return plus
div-by-zero and float-arith abort paths.

### 1.3 feat(codegen-native): __relon_jump_to_recorder real implementation

`crates/relon-codegen-native/src/trace_install.rs` —
the helper grew from a debug-counter stub to a real recording
driver:

1. Bump diagnostic counter (preserved for smoke-test assertions).
2. Look up `RECORDING_REGISTRY[fn_id]` in the per-thread
   `RecordingRegistration` map. Absent → return (cranelift-generic
   handles the cold path).
3. Materialise `(u64, IrType)` arg pairs by reading
   `param_tys.len()` entries off `args_ptr` (null → zeroed slots).
4. Spin up `RecorderState` + `TraceRecordingEvaluator`, walk the
   body, capture `RecordingOutcome`.
5. On `Recorded`: drive the optimizer → emitter → JIT install
   pipeline via the global `TraceJitState`. On `Aborted`: log +
   return; the counter stays saturated.
6. Subsequent invocations short-circuit when a trace is already
   installed for `fn_id`.

Public surface additions:
- `RecordingRegistration { body, param_tys }`
- `register_recording(fn_id, reg) -> Option<RecordingRegistration>`
- `clear_recording(fn_id) -> Option<RecordingRegistration>`
- `recording_registration_count() -> usize`

4 new smoke tests cover: no-registration noop, const-trace install
+ invoke + idempotent re-entry, registry round-trip, abort on
`Op::CallNative`.

### 1.4 feat(trace-abi): re-export TraceHookFn + with_hooks ctor

`crates/relon-trace-abi/src/context.rs` — `TraceContext::with_hooks`
takes a pre-populated `HostHookTable` so hosts can wire trace
runtime helper addresses up front before invoking a trace. The
hook signature `TraceHookFn = unsafe extern "C" fn(*mut
TraceContext, u32)` is now re-exported at the crate root.

The codegen-native side adds `default_host_hooks()` returning a
table with `save_deopt` wired to a shim around
`__relon_trace_save_deopt`. `resolve_call` / `inline_cache_lookup`
stay `None` because their canonical signatures have wider arg /
return shapes than the uniform `TraceHookFn`; widening the hook
type is an M5 decision.

The cranelift trace emitter continues to resolve the hooks via
`JITBuilder::symbol` (direct extern call); the
`HostHookTable.save_deopt` slot is now reachable for hosts that
want to inspect / log deopt telemetry without re-deriving the
helper address from the JIT module.

### 1.5 feat(codegen-native): invoke_with_fallback deopt path

`TraceJitState::invoke_with_fallback(fn_id, args_ptr, slot_count,
fallback)`:

1. Lookup installed trace; absent → run `fallback`.
2. Allocate a `TraceContext` with `default_host_hooks()` pre-wired.
3. Invoke trace; route on `TraceEntryStatus`:
   - `Success`: return `ctx.result_slot`.
   - `GuardFailed`: log + fall back. (v6-γ M4 conservative cut:
     re-run from the top rather than partial-resume from
     `snapshot.external_pc`. Correctness-first; M5 polishes
     partial-resume.)
   - `Aborted`: invalidate the trace + fall back.

`TraceJitState::invalidate_trace(fn_id) -> Option<Arc<JITedTraceFn>>`
drops the trace on demand.

4 new smoke tests cover success path, no-trace fallback,
invalidate behaviour, and the populated `save_deopt` slot.

### 1.6 feat(harness): diff_test_3way (tw / aot / trace)

`crates/relon-test-harness/src/three_way.rs` — three-way runner.

- Tree-walk reference via `Backend::TreeWalk`.
- Cranelift-AOT reference via `Backend::CraneliftAot`.
- Trace-JIT path: pattern-match the source against the
  Phase-1 arith envelope; build a `TaggedOp` body for `LocalGet(0)
  LocalGet(1) <op> Return`; register via `register_recording`;
  fire `__relon_jump_to_recorder`; invoke through
  `invoke_with_fallback`.

`ThreeWayResult` variants:
- `AllAgree(Value)`: every backend produced the same value.
- `AllTrap`: every backend trapped equivalently.
- `TraceJitNotApplicable { baseline, reason }`: tw + aot agreed;
  trace-JIT couldn't synthesise (today most corpus cases).
- `CraneliftUnsupported { tree_walk, reason }`: cranelift bounced.
- `Mismatch { tree_walk, cranelift, trace_jit }`: at least two
  backends disagreed.

`ThreeWayResult::is_pass()` reports whether the outcome is
acceptable for harness purposes.

11 cases in `tests/three_way_smoke.rs` exercise the arith envelope
(positive / negative / zero / identity inputs across +/-*//) plus
a rich-source fallback canary. Every case reaches `AllAgree`.

## 2. Key design decisions

1. **Helper signature stays void.** The codegen prologue emits a
   `extern "C" fn(u32, *const u64)` call and immediately returns
   the entry's sentinel zero. The plan's "result = run; return
   result" pseudo-Rust was rephrased so M4 keeps the existing ABI
   shape: install is a fire-and-forget side effect; the current
   invocation returns sentinel zero (the host's existing code path
   for hot triggers), and **subsequent** invocations either hit
   the installed trace or fall back via `invoke_with_fallback`.
2. **Thread-local IR registry.** `RecordingRegistration` lives in
   a `RefCell<HashMap>` per thread, matching the design doc's
   §3.4 "per-thread recorder state machine" decision. Avoids
   another `RwLock` and lets concurrent harness tests carve out
   disjoint `fn_id` ranges without coordination.
3. **Synthetic external_pc.** The recorder doesn't see real
   instruction pointers (the IR walker, not the cranelift-generic
   backend, drives the recording today). A monotone u64 counter
   per `RecorderState` is good enough to keep `GuardSite.deopt_pc`
   ids unique. Production hosts will call
   `recorder.set_next_external_pc(real_ip)` once the cranelift
   backend gets the per-op IP table wired through.
4. **`record_arith` was already correct** — the bug was lower
   down. The recorder's `apply_outcome` flowed every guard through
   `emit_guard`, which appended the `TraceOp::Guard` but never
   called `record_guard` on the buffer. Fixing the shared helper
   covers Add/Sub/Mul/Div *and* the TypeCheck / BoundsCheck paths
   simultaneously.
5. **Conservative deopt fallback (re-run rather than
   partial-resume).** v6-γ M4 takes the correctness-first cut:
   `GuardFailed` runs the fallback closure (typically the
   generic-backend re-run) from the top. The `snapshot.external_pc`
   is recorded into `ctx.deopt_state` but not consumed. M5 polishes
   the partial-resume path now that the protocol works end-to-end.
6. **HostHookTable parallel slot.** The cranelift emitter still
   resolves `save_deopt` via `JITBuilder::symbol` for performance
   (direct call, no extra indirection). The hook table is
   populated in parallel so a future emitter revision can switch
   to `call_indirect` through the context without an ABI break,
   and so hosts can inspect "did this trace deopt?" by reading
   `ctx.host_hooks.save_deopt.is_some()` rather than re-deriving
   the symbol address.

## 3. Gate numbers

- `cargo build --workspace` — clean.
- `cargo test --workspace` — **1693** passing (baseline 1654 + 39
  new; target was ≥ 1669).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` —
  clean.

### 3.1 Per-file test count

| File                                                    | Tests added |
|---------------------------------------------------------|-------------|
| `relon-trace-recorder/tests/orphan_guard_fixed.rs`      | 7           |
| `relon-codegen-native/src/trace_recording.rs` (unit)    | 8           |
| `relon-test-harness/tests/trace_jit_smoke.rs`           | 8           |
| `relon-test-harness/src/three_way.rs` (unit)            | 5           |
| `relon-test-harness/tests/three_way_smoke.rs`           | 11          |
| **Total**                                               | **39**      |

## 4. 3-way diff smoke pass rate

**11 / 11** (100 %).

All cases hit `ThreeWayResult::AllAgree(Value::Int(...))` with the
expected concrete result. The arith envelope's edge cases
(negatives, zero, identity, large factors) all produced
bit-identical results across tree-walk, cranelift-AOT, and
trace-JIT.

## 5. Residual TODO (M5 scope)

- Three-way **corpus** runner: extend `corpus_differential.rs` to
  feed every corpus case through `diff_test_3way`. Most will
  surface as `TraceJitNotApplicable` (the synthesis envelope only
  covers ArithControl two-arg arith); future tranches widen the
  synthesiser as the recorder grows new ops.
- **Partial-resume from `snapshot.external_pc`.** The deopt
  fallback today re-runs the entry from the top. M5 wires up the
  resume-from-IP path via the cranelift-generic backend's op
  table.
- **Hot loop micro-bench.** Target <5 ns / iter for a `10^6` loop
  body once the trace is installed. v6-γ M4 hasn't bench'd; the
  install path itself is a slow-path trigger so bench numbers
  need a long-running entry that exercises the warm trace fn.
- **Real arg ptr in cranelift prologue.** The
  `__relon_jump_to_recorder` helper accepts both real and null
  `args_ptr` today; the prologue still passes null per
  `emit_hot_counter_inject`. M5 bumps the prologue to pack the
  entry-function args into a stack-allocated `u64[]` before
  calling the helper.
- **Widen `TraceHookFn` signature.** Today the uniform
  `(*mut TraceContext, u32)` shape forces `resolve_call` and
  `inline_cache_lookup` to stay `None` in the default hook
  table. M5 decides whether to add a return-typed variant or
  promote all hooks to a wider tuple.

## 6. Commit log (this stage)

```
ee4d64b feat(harness): diff_test_3way (tw / aot / trace)
2b7cb1e feat(trace-abi): re-export TraceHookFn + with_hooks ctor
1b87ab2 feat(codegen-native): __relon_jump_to_recorder real implementation
2387df4 feat(codegen-native): TraceRecordingEvaluator IR walker
0754a3f fix(trace-recorder): record_guard called inside append path
```

`git diff --stat f72906a..HEAD` (pre-doc commit):

```
11 files changed, 1864 insertions(+), 29 deletions(-)
```

## 7. Risks + mitigations carried into M5

| Risk                                                    | Severity | Mitigation                                                       |
|---------------------------------------------------------|----------|------------------------------------------------------------------|
| Partial-resume off-by-one between trace + generic IP    | Medium   | M5 wires through the cranelift-generic op table; smoke tests pin |
| `register_recording` thread-locality vs cross-thread JIT | Low      | Single-threaded harness today; M5 audits when multi-thread lands |
| `__relon_jump_to_recorder` re-entrancy under contention | Low      | Counter is non-atomic by design; double-install short-circuits   |
| Hook table widening breaks ABI                          | Medium   | New `TraceHookFn2`-style variant; existing one stays unchanged   |
