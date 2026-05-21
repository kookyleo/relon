# RFC — v6-δ M2-B Bytecode Backend Investment

Date: 2026-05-21
Status: planning + Phase 1 / 2 / 3 land
Owner: bytecode VM track
Worktree: `.claude/worktrees/agent-ac8a6a5af256fa8c2`

## 1. Audit (current state @ ea29a7d)

**Module shape** — `relon-bytecode/src/{lib,op,compile,vm,evaluator}.rs`
ships the M2-A scaffold: stack-based VM with per-function `ir_pc_map`
plus the `stack_recipe` table for mid-expression resume.
`BytecodeEvaluator` implements `Evaluator`, wired into
`relon::new_evaluator` as `Backend::Bytecode`.

**`BcOp` table** — covers `Const{I64,I32} / LocalGet / LocalSet /
Add..Mod / Eq..Ge / Jump / JumpIf* / Return / Trap`. Missing vs
`relon_ir::Op` (~74 variants): `ConstBool / ConstF64 / ConstString /
ConstList* / LetGet / LetSet / LoadField / StoreField /
DictGetByStringKey / ListGetByIntIdx / BitAnd / If / Load*Ptr /
AllocRootRecord / AllocSubRecord / StoreFieldAtRecord /
PushRecordBase / EmitTailRecordFromAbsoluteAddr / Call / CallNative /
CheckCap / ReadStringLen / Select / Load*AtAbsolute / Store*AtAbsolute
/ MemcpyAtAbsolute / Block / Loop / Br / BrIf / BrTable /
AllocScratch* / MakeClosure / CallClosure / stdlib *TableAddr tags`.

**`ir_pc_map` / `ExternalPc`** — real impl (not placeholder).
`BcFunction.ir_pc_map: Vec<ExternalPc>` mirrors IR PC per bytecode
index; `bc_index_for_pc` answers resume queries. `StackOrigin
{ Local | Const | Snapshot }` recipe per bc index drives
mid-expression resume; `resume_from_pc` / `resume_from_snapshot`
both work for the scalar envelope.

**Capability vtable** — `CapabilityVtable { grants: Vec<bool> }`,
grant-tracking only, no host-fn slot payload. M2-A reason:
bytecode has no `CallNative` op so a slot payload would be dead
weight. P0-B unified cranelift + tree-walker policy through
`relon_eval_api::CapabilityGate` and explicitly deferred bytecode
wiring to M2-B.

**Bench 4-way row** — `relon-test-harness::four_way::diff_test_4way`
exists and runs `Backend::Bytecode`. The 4-way runner is **not**
wired into any benchmark in `crates/relon-bench/benches/`; the
existing benches never invoke the bytecode VM.

## 2. M2-B scope — by ROI

| # | Item | Workload | Notes |
|---|------|----------|-------|
| 1 | **CapabilityGate wire-up** (no native dispatch yet) | S | landed this phase — trait hook on vtable + ctor that consults a `&dyn CapabilityGate` |
| 5 | **4-way bench row** activation | S | wire `Backend::Bytecode` into `cranelift_aot_vs_tree_walk` (rename to `four_way`) for scalar corpus subset |
| 3 | **Trace-JIT hot counter injection** | M | per-BcFunction call counter + threshold trip → recorder bridge; covers the deopt-resume entry shape |
| 6 | **Capability gate enforcement at dispatch** | S | depends on #1 + a `BcOp::CallNative` op; gate check fires on every guarded op |
| 4 | **Deopt resume via ir_pc_map → IR position re-entry** | L | already 80 % built (M2-A scaffold); finish the bridge into `TraceJitState::invoke_with_resume` + cross-validate against tree-walker |
| 2 | **IR coverage expansion** (list / dict / string / stdlib) | L | needs buffer-protocol arena ops + memory stdlib bodies; this is the dominant cost |

## 3. Phase split

- **Phase 1 (landed)** — #1: vtable accepts `Option<Arc<dyn CapabilityGate>>` via `cap_vtable.set_gate(...)`. No behaviour change for callers that don't set a gate; the trait hook is parked for #6.
- **Phase 2 (landed)** — gate-consult helpers + dispatch-time pre-check + `BcOp::Trap(CapabilityDenied)` enrichment with first-denied-bit. Five new helper API entries on `CapabilityVtable`.
- **Phase 3 (landed)** — IR coverage expansion: four new `BcOp` variants (`CallNative`, `CheckCap`, `CallStdlibScalar`, `ListLen`) with per-call-site capability consult routed through the phase-2 helpers. `BcVmError::NativeNotImplemented` carries the phase-4 host-fn registry gap. The compile pass lifts IR `Op::CallNative` / `Op::CheckCap` from the unsupported pile into real bytecode emit. See `docs/internal/review-improvement-133-bytecode-phase3-2026-05-21.md` for the stage report.
- **Phase 4a (landed)** — host-fn registry on `CapabilityVtable` keyed by `import_idx` (`HashMap<u32, Arc<dyn RelonFunction>>`). `BcOp::CallNative` now: consult gate → resolve registry slot → pop args (Phase-4a scalar lane: all args travel as `Value::Int`) → invoke `RelonFunction::call` with a minimal `BytecodeNativeFnCaps` stub → encode return per `ret_ty`. Two new error envelopes: `HostFnError { import_idx, reason }` for host-side failures and `HostFnReturnTypeMismatch { import_idx, expected, found }` for non-scalar returns. Unregistered slots keep the legacy `NativeNotImplemented` fallback so the differential harness's bounce shape stays stable. See `docs/internal/review-improvement-141-bytecode-phase4a-2026-05-21.md`.
- **Phase 4b (planned)** — real list/dict ops + the buffer-protocol memory model that unlocks per-arg-type lanes for host fns, hot-counter + trace-JIT bridge (the original phase-3 deliverable, deferred so the IR coverage work could land first). 4-way bench row activation and `cmp_lua_dict_list_trace` integration follow once the wider memory model is wired.

Each phase ≤ M work; phase 4 splits into 4a (host-fn registry; landed) + 4b (list/dict ops; deferred).

## 4. Risks / unknowns

- **VM perf vs tree-walker**: untested. Bytecode VM uses `match` dispatch on `&BcOp`. If we measure < 1× tree-walker, items #2 + #6 lose value. **Mitigation**: phase 2 lands the 4-way bench row first.
- **`Op::Call(closure)` modelling**: bytecode VM has no call-frame stack; `compile_function_in_module` inlines simple callees. Real `MakeClosure` / `CallClosure` need either a frame stack (deferred to M3) or arity-limited inlining (phase 4).
- **3-way → 4-way bench cost**: adding bytecode row to existing benches adds wall-clock cost. **Mitigation**: gate the row behind a cargo feature.
- **Capability timing diff**: cranelift fires at vtable-build, tree-walker at dispatch. Bytecode will fire at dispatch (matches tree-walker); P0-B doc already noted the intentional diff.

## 5. Phase 1 commit shape

`relon-bytecode`:

- `CapabilityVtable` gains an optional `Arc<dyn CapabilityGate>` field with `set_gate` / `gate` accessors. Grant table stays; phase 2 consults gate first, falls back to grants.
- `BytecodeEvaluator::with_capability_gate(gate)` builder threads the gate into the default VM config; subsequent `run_main` / `resume_from_*` inherit it.
- Module-level doc: "M2-B phase 1: capability gate hook ready; native dispatch follows in subsequent phase."

No new ops, no behaviour change for callers that don't opt in.
