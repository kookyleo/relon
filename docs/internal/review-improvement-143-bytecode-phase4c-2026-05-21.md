# Review improvement #143 — bytecode M2-B phase 4c stage report

## Scope

Bring the bytecode VM under the same trace-JIT hot-counter machinery
the cranelift backend uses, so a hot bytecode loop drives recording →
JIT install. Sub-steps 1+2 (counter + dispatch wire-up) and sub-step
3 (adapter + e2e) landed; deopt-resume routing is phase 4c-cont.

## Hot counter design

Mirrors `relon_trace_jit::counter::HotCounter`:

- `HotCounter` = `Cell<u32>` + threshold. `record()` returns Cold /
  Heating(n) / HotTrigger / AlreadyHot; saturates on crossing.
- Per-thread `HOT_COUNTERS: HashMap<u32, HotCounter>` inside the
  bytecode crate. Shadows cranelift's `RELON_HOT_COUNTERS` but stays
  cranelift-free (wasm32 still compiles).
- `BcFunction` gets optional `fn_id` (host stamps it); without one,
  prologue is inert.
- `BcVmConfig` gets `hot_trigger: Option<HotTraceTriggerHandle>` +
  `hot_threshold: u32` (default 1000). Threshold 0 panics.
- Dispatch prologue runs only when `start_bc_idx == 0` — partial-
  resume re-entries skip the bump so deopt bounces never retrigger
  the recorder.

## Jump-helper path

Bytecode side owns no cranelift; the trigger is a `HotTraceTrigger`
trait. `CraneliftHotTrigger`
(`crates/relon-codegen-native/src/bytecode_bridge.rs`) implements it
by indirecting to `__relon_jump_to_recorder` — the same helper the
cranelift entry prologue calls. Wrapped in `catch_unwind` so a
recorder-side panic is logged via tracing instead of aborting the
bytecode dispatch loop. `BytecodeEvaluator` gains `with_fn_id` /
`with_hot_trigger` / `with_hot_threshold` builders.

## Recorder reuse decision

Reused the existing IR walker (`TraceRecordingEvaluator`) — no new
bytecode → TraceOp lowering rule. Rationale: the bytecode artefact
was already lowered from the same IR module the recorder reads; if
the host registers the IR body under `fn_id` (via
`register_recording`) alongside compiling the bytecode (same id), the
trigger path drives the existing pipeline verbatim. This avoids
double maintenance for two source-of-truth lowerings.

## Tests

- `relon-bytecode/src/hot_counter.rs::tests` — 4 unit tests on the
  counter math + trait shape.
- `relon-bytecode/tests/hot_counter_dispatch.rs` — 6 integration
  tests: threshold trigger fires exactly once, saturation, fn_id-less
  / trigger-less inertness, partial-resume skip, independent slot
  tracking, arg propagation to `on_hot`.
- `relon-codegen-native/src/bytecode_bridge.rs::tests` — 2 adapter
  smoke tests (unregistered fn_id falls through cleanly + trait-
  object wiring).
- `relon-test-harness/tests/bytecode_hot_counter_e2e.rs` — 2 e2e
  tests: full pipeline (register IR → bytecode VM bumps → trigger →
  recorder → JIT install → trace fn resident) + saturated-slot short
  circuit (5 invocations → exactly 1 helper call).

Workspace test count rose from the 2130 baseline to 2144. Gate:
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, and `cargo check --target wasm32-unknown-unknown -p
relon-wasm` all pass.

## Deopt resume status

Parked for phase 4c-cont. The existing M2-B partial-resume path
(`BytecodeEvaluator::resume_from_pc`) already routes a deopt'd
snapshot back into bytecode dispatch via `start_bc_idx > 0`; the
phase 4c prologue intentionally skips the hot-counter bump on those
re-entries so the recorder never retriggers off a guard miss. What
remains is the **dispatcher-side switch**: when an installed trace
exists for `fn_id`, the bytecode VM should bypass its own dispatch
loop and invoke the trace fn directly (mirror of
`CraneliftAotEvaluator::run_main` routing through
`TraceJitState::invoke_with_resume`). That switch + a corpus-wide
verification of "bytecode→trace→deopt→bytecode" handoff lands in
phase 4c-continuation.
