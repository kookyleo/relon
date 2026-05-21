# Review improvement #146 — bytecode M2-B phase 4c-cont stage report

## Scope + completion

Phase 4c left three critical items; phase 4c-cont lands two, parks
the third for follow-up.

| Sub-task | Status |
| --- | --- |
| A — dispatcher switch (detect installed trace + bypass loop) | **Done** |
| B — full deopt → bytecode handoff via dispatcher switch | **Done** |
| C — drop `Op::ConstListInt` / `Op::ConstString` length-fold | Deferred to 4c-cont-2 |

## Sub-task A: dispatcher switch design

New module `relon_bytecode::trace_dispatch` exposes
`InstalledTraceLookup` (trait, `Send + Sync`) +
`TraceInvokeOutcome` (`NoTrace` / `Success { result }` /
`Deopt { snapshot }`). `BcVmConfig` grows `trace_lookup: Option<Arc<…>>`;
`BytecodeEvaluator::with_trace_lookup` is the builder.

`BytecodeEvaluator::run_main` consults the lookup at entry when both
`func.fn_id` and `trace_lookup` are set:

- `NoTrace` → fall through to the existing dispatch loop (packed
  args re-used to avoid double-packing).
- `Success { result }` → `pack_trace_result(result)` decodes the
  `TraceContext::result_slot` value per the return schema and skips
  the dispatch loop entirely. This is the user-visible win on hot
  loops (mirror of the cranelift entry-fn prologue + IC jump).
- `Deopt { snapshot }` → route through `resume_from_deopt`
  (sub-task B alias for `resume_from_snapshot`).

The native bridge `CraneliftTraceLookup`
(`relon_codegen_native::bytecode_bridge`) wraps
`TraceJitState::invoke_with_resume`. The bytecode crate stays
`forbid(unsafe_code)`: the bridge owns the `unsafe` block and
the `DeoptStateSnapshot` rebuild (it intentionally does not impl
`Clone`; we copy the three fields the resume path consults —
`guard_pc` / `external_pc` / `ssa_slots_copy` / `value_stack_copy`).
A `catch_unwind` wraps the call so any recorder/install pipeline
panic degrades gracefully to `NoTrace`.

## Sub-task B: deopt marshalling + handoff

The marshalling itself already shipped in M2-B: the `stack_recipe`
table per `bc_idx` rebuilds the operand stack from a mix of locals /
const / snapshot fragments, and `BcFunction::bc_index_for_pc` maps
the trace's `external_pc` to the matching bytecode index. Sub-task B
adds the public `BytecodeEvaluator::resume_from_deopt` entry point
(alias for `resume_from_snapshot`) so the dispatcher switch's
`Deopt` arm has an explicit named call site, and proves the round
trip is observable through `run_main` alone (no manual
`invoke_with_resume` orchestration).

## Tests

- `relon-bytecode/src/trace_dispatch.rs::tests` — 2 unit tests on
  the outcome enum + trait shape.
- `relon-bytecode/tests/trace_dispatch_switch.rs` — 5 integration
  tests via mock lookup: each outcome variant routes correctly,
  `fn_id` absence makes the switch inert, repeat invocations all
  consult.
- `relon-codegen-native/src/bytecode_bridge.rs::tests` — 2 native
  adapter smokes (unregistered fn_id → NoTrace + trait-object
  plumbing).
- `relon-test-harness/tests/bytecode_trace_dispatch_switch_e2e.rs`
  — 2 e2e: bypass-after-install + 10-call hot-loop bypass counting.
- `relon-test-harness/tests/bytecode_trace_deopt_handoff_e2e.rs`
  — 2 e2e: cold-args overflow handoff propagating
  `NumericOverflow`, mixed-workload outcome accounting
  (1 NoTrace + 3 Success + 1 Deopt).

Workspace test count rose from 2144 baseline to **2157**. Gate:
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, `cargo test --workspace`, and
`cargo check --target wasm32-unknown-unknown -p relon-wasm` all
pass.

## Follow-up: sub-task C

Dropping the `Op::ConstString` / `Op::ConstListInt` length-fold
needs the compile pass to track per-slot value class (StringHandle
vs ListHandle vs Int) so `visit_read_string_len` can dispatch to
`BcOp::StrLen` (real handle path) vs `BcOp::ListLen` (still a
length-fold witness today). Splitting hits non-trivial state in
`CompileState` and is left for a focused phase 4c-cont-2 commit so
the four-way harness regression risk stays bounded. The current
length-fold path remains functionally correct; the cost is missed
trace coverage for `Op::ConstString`-producing call sites the
recorder would otherwise see as real handles.
