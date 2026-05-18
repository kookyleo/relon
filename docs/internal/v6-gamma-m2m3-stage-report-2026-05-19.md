# v6-γ M2 + M3 Stage Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Status: M2 + M3 delivered; M4 deopt + M5 differential remain.
Companion: `docs/internal/v6-gamma-integration-plan-2026-05-18.md`

---

## 0. TL;DR

- M2 (HotCounter inject + `__relon_jump_to_recorder` host helper):
  **DONE**.
- M3 (`jit_compile_trace_for_fn` end-to-end pipeline + runtime helper
  registration in the cranelift JIT module): **DONE**.
- 16 new integration tests (`trace_jit_smoke.rs`) covering counter
  inject + threshold cross-over + per-fn isolation + buffer →
  optimizer → emitter → JIT install → invoke end-to-end.
- Gate green: 1654 passing tests (baseline 1632, ≥ 10 new target
  cleared); `cargo build` / `cargo test` / `cargo clippy
  --all-targets -D warnings` / `cargo fmt --check` /
  `cargo build --target wasm32-unknown-unknown -p relon-wasm` all
  clean.

## 1. What landed

### 1.1 New module: `crates/relon-codegen-native/src/trace_install.rs`

Public surface:

- `RELON_HOT_THRESHOLD = 10` (matches LuaJIT default; §1.2 of the
  v6-γ design doc).
- `MAX_FN_ID = 1024` slot capacity.
- `HOT_COUNTERS_SYMBOL = "__relon_hot_counters"` (reserved name; the
  current implementation inlines the table base via `iconst.i64`
  but the symbol is kept for future object-cache rebinding).
- `hot_counters_base() -> *mut u32` — process-stable pointer at the
  start of the counter table.
- `hot_counter_peek(fn_id)` / `hot_counter_reset(fn_id)` /
  `hot_counter_reset_all()` — observability + test isolation.
- `__relon_jump_to_recorder(fn_id, args_ptr)` — `extern "C"` host
  helper invoked from the cranelift prologue when a counter
  saturates. v6-γ M2/M3 cut: bumps a thread-local diagnostic
  counter + emits a `tracing::debug!` so smoke tests can prove the
  cranelift prologue path actually executed. Replacing this stub
  with a full IR-walk-into-recorder driver is M4 work.
- `jump_helper_call_count()` / `reset_jump_helper_call_count()` —
  per-thread cell tests assert against.
- `TraceJitState` — registry holding `HashMap<u32,
  Arc<JITedTraceFn>>` behind an `RwLock` plus the
  `jit_compile_trace_for_fn` / `jit_compile_buffer_for_fn` /
  `install_trace` / `lookup_trace` API.
- `JITedTraceFn` — opaque entry holding the JIT-finalised function
  pointer and its owning `JITModule`. Exposes `invoke(*mut
  TraceContext, *const u64) -> TraceEntryStatus`.
- `TraceJitError` — typed errors for the four pipeline failure
  modes (`RecorderAbort`, `Emit`, `Module`, `FnIdOutOfRange`).
- `build_trace_jit_module` / `register_trace_runtime_symbols` —
  factored out so the codegen entry-fn JIT module and the trace
  install JIT module both register the same four symbols
  (`__relon_trace_save_deopt`, `__relon_trace_resolve_call`,
  `__relon_trace_inline_cache_lookup`, `__relon_jump_to_recorder`).
- `global_trace_jit_state()` — `OnceLock`-backed singleton hooked
  from the host helper.

### 1.2 Codegen prologue inject

`crates/relon-codegen-native/src/codegen.rs`:

- New `emit_hot_counter_inject` helper. When
  `SandboxConfig.trace_jit_fn_id == Some(fn_id)`, the entry block
  is split into `hot_block` / `normal_block`. The prologue emits
  `load.i32 / iadd_imm / store.i32 / icmp_imm.uge` then `brif`.
- Hot path: `iconst` the fn pointer of `__relon_jump_to_recorder`,
  `call_indirect` it with `(fn_id, null)`, then `return 0` of the
  entry's return type (sentinel that bypasses the trapped-on-zero
  trampoline).
- Normal path: the builder is left positioned on the (sealed)
  normal block; the existing entry body codegen flows unchanged.
- Backward-compat: `trace_jit_fn_id = None` skips the inject
  entirely, so every existing test path keeps its v5-γ stage 2
  shape.

### 1.3 Runtime helper registration

The cranelift entry-fn JIT module now calls
`trace_install::register_trace_runtime_symbols(&mut jit_builder)`
right before `JITModule::new`. This registers the three trace
runtime helpers plus the codegen-native-side jump helper so the
emitted IR can `call_indirect` through the helper addresses without
relying on `rdynamic` symbol resolution.

### 1.4 Tests

`crates/relon-test-harness/tests/trace_jit_smoke.rs` — 16 cases:

| # | Name                                          | Coverage                                                 |
|---|-----------------------------------------------|----------------------------------------------------------|
| 1 | `hot_counter_below_threshold_returns_value`   | counter increments, no helper call, value untouched      |
| 2 | `hot_counter_triggers_at_threshold`           | threshold crossing → helper fires once, sentinel return  |
| 3 | `hot_counter_post_threshold_keeps_firing`     | saturated counter keeps routing through helper           |
| 4 | `hot_counters_are_per_fn_id`                  | separate fn_ids increment independently                  |
| 5 | `no_trace_jit_no_helper_calls`                | `trace_jit_fn_id=None` leaves helper untouched           |
| 6 | `pipeline_compiles_const_return_trace`        | recorder → optimizer → emitter → JIT for const return    |
| 7 | `pipeline_install_lookup_round_trip`          | `install_trace` / `lookup_trace` consistency             |
| 8 | `pipeline_invoke_returns_success`             | JIT entry returns `TraceEntryStatus::Success`            |
| 9 | `pipeline_writes_result_slot`                 | trace populates `ctx.result_slot`                        |
| 10 | `pipeline_aborts_unsupported_op`             | `Op::CallNative` aborts → typed `TraceJitError`          |
| 11 | `pipeline_out_of_range_fn_id_errors`         | `fn_id >= MAX_FN_ID` → typed `FnIdOutOfRange`            |
| 12 | `pipeline_compiles_add_trace`                | buffer-built `Add(11,22)` trace → JIT → invoke = 33      |
| 13 | `pipeline_compiles_mul_trace_via_buffer`     | buffer-built `Mul(6,7)` trace → JIT → invoke = 42        |
| 14 | `pipeline_chained_trace_buffer_install_invoke`| install → lookup → invoke round-trip via registry        |
| 15 | `pipeline_load_store_trace_via_buffer`       | `Load` op from a heap-backed slot → JIT → invoke         |
| 16 | `global_state_singleton_is_stable`           | `OnceLock` singleton address stable across calls         |

Five additional unit tests live in
`trace_install::tests` (counter pointer stability, peek/reset round
trip, `jit_compile_trace_for_fn` success path, install round trip,
`record_program_into_state` collect path, fn_id range check).

## 2. Key design decisions

1. **Non-atomic counter** — every prologue emits `load.i32 / iadd_imm
   / store.i32 mem_flags=trusted`. Multi-thread races on the same
   fn_id may delay a trigger by one or two iterations but never
   cause UB; the storage is plain `u32` in a single `UnsafeCell`
   wrapper marked `Sync`.
2. **Inlined base address** — the prologue folds the table base
   into an `iconst.i64`. The plan also mentions a
   `symbol_value RELON_HOT_COUNTER_BASE` form; we picked the
   simpler inline because the table is process-stable and no
   relocation buys anything for the in-process JIT path. The
   `HOT_COUNTERS_SYMBOL` constant is reserved so a future
   cranelift-object code path can re-bind by name without
   reshaping the prologue.
3. **fn_id opt-in via `SandboxConfig`** — adding a fourth bool
   would have toggled every existing test; tagging the prologue
   with an explicit `Option<u32>` keeps the v5 / v6-γ paths
   distinguishable and lets the smoke tests assign disjoint slot
   ids to avoid parallel-cargo races.
4. **`__relon_jump_to_recorder` as a thin stub** — the v6-γ M2/M3
   surface needed observable proof that the prologue path
   executes. The stub bumps a per-thread cell + emits a
   `tracing::debug!`. The real "interpret IR + record" path is M4
   work and was explicitly de-scoped here per the
   `if context can't hold, commit M2 + leave M3 for follow-up`
   pivot in the plan; we got further (M3 pipeline is wired
   end-to-end on the synthetic op path) so the only residual gap
   is the live IR-walk lifting.
5. **Recorder still emits orphan guards** — the pre-integration
   recorder appends a `Guard` op but doesn't call
   `TraceBuffer::record_guard`, so the emitter produces
   `EmitError::OrphanGuardOp` for any op with `guards_after !=
   []`. Fixing this is queued for M4; the M3 smoke tests
   side-step it by hand-building a `TraceBuffer` for the
   non-trivial cases (Add, Mul, Sub, Load) via the new
   `jit_compile_buffer_for_fn` entry point.

## 3. Gate numbers

- `cargo build --workspace` — clean
- `cargo test --workspace` — **1654** passing (baseline 1632 + 22 new)
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo fmt --all -- --check` — clean
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` — clean

## 4. End-to-end confirmation

The buffer-built Add(11,22) trace (`pipeline_compiles_add_trace`) is
the canonical "trace runs through every stage" demonstration:

```rust
let mut buffer = TraceBuffer::new();
let a = buffer.fresh_ssa();
let b = buffer.fresh_ssa();
let sum = buffer.fresh_ssa();
buffer.append(TraceOp::ConstI64(a, 11));
buffer.append(TraceOp::ConstI64(b, 22));
buffer.append(TraceOp::Add(sum, a, b));
buffer.append(TraceOp::Return(sum));

let trace_fn = state.jit_compile_buffer_for_fn(0, buffer)?;
let mut ctx = TraceContext::with_capacity(64);
let status = unsafe { trace_fn.invoke(&mut ctx, std::ptr::null()) };
// status == Success, ctx.result_slot == 33.
```

The trace went through `OptimizerPipeline::default_pipeline().run`
(6 passes), the cranelift `TraceEmitter`, a fresh `JITModule`'s
`declare_function` + `define_function` + `finalize_definitions`, and
finally an indirect `call_indirect` through the resolved fn pointer.
`ctx.result_slot` ending up at 33 confirms the emitter's `Return`
lowering wrote through the load-bearing byte offset in the shared
`relon-trace-abi::TraceContext`.

## 5. Bench

No micro-bench delta in this stage. The HotCounter prologue adds
~3-5 ns warm-invoke overhead per the plan estimate; an end-to-end
trace-vs-generic compare needs M4 deopt machinery (so the trace can
fall back on guard failure) before producing meaningful numbers.
Pushed to M5.

## 6. Residual TODO

- M4 — wire `__relon_trace_save_deopt` through the
  `TraceContext::host_hooks` table and exercise a real deopt round
  trip from a JIT-emitted guard back into the cranelift-generic
  backend. The pre-built emitter already calls the helper at the
  `deopt_block` site; the missing piece is having the host install
  the symbol address into `host_hooks.save_deopt` before invoking
  the trace.
- M4 — make the recorder call `TraceBuffer::record_guard` alongside
  every `Guard` op append so the emitter's per-pc lookup succeeds
  and the `pipeline_compiles_add_trace` test can drive the
  recorder path (rather than the hand-built buffer path) without
  tripping `EmitError::OrphanGuardOp`.
- M4 — make `__relon_jump_to_recorder` actually drive the recorder
  against the cranelift-generic backend's live IR stream so the
  end-to-end "10 calls → trace installed → 11th call goes through
  JIT" path proves itself in `cargo test`.
- M5 — three-way differential corpus (tree-walk vs cranelift-aot
  vs trace-jit). Re-use the existing 52-case corpus with new
  per-case annotations (`min_iterations_for_trace_install`,
  `trace_jit_expectation`).
- M5 — hot loop micro-bench `10^6` iters target <5 ns/iter.

## 7. Commit log (this stage)

```
c7f5ba2 feat(codegen-native): v6-gamma TraceJitState + hot counter pipeline
d704d4b feat(codegen-native): inject HotCounter prologue + trace runtime symbols
84bb59f feat(codegen-native): jit_compile_buffer_for_fn + smoke tests
```

`git diff --stat 968f08a..HEAD` (after this report's doc commit):

```
8 files changed, 717 insertions(+), 7 deletions(-)
```
