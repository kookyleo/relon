# Review Improvement #166 — bytecode `from_source` full capability-gate activation

**Date**: 2026-05-22
**Worktree**: `/ext/relon/.claude/worktrees/agent-a6b5470c1dfddae83`
**Base**: `db9980d` (local `main` HEAD — no `fetch origin`)
**Branch**: `worktree-agent-a6b5470c1dfddae83`

## Context

#129 (phase 1/2) and #141 (phase 4a) landed the bytecode VM's
`CapabilityGate` plumbing — `CapabilityVtable::set_gate`, the
pre-dispatch sweep over `grants`, the per-op consult on
`BcOp::CallNative` / `BcOp::CheckCap`, and the host-fn registry. The
mechanism was in place but the `from_source` pipeline still produced
op streams without capability-sensitive ops (scalar arith / cmp /
control flow only), so a host that installed a gate observed **zero**
consults on the production scalar workload. The two stage reports both
called this out explicitly as deferred wire-up to avoid a fake-check
shape.

## Option Picked — B (sensitive-op flag)

Walked the three candidates:

* **A** — unconditional entry-time sweep over every declared
  `CapabilityBit`. Easy but breaks the historical zero-overhead
  posture on the scalar `from_source` envelope; every invoke pays six
  virtual calls.
* **B** — compile-pass scan flags functions whose op stream contains
  `BcOp::CallNative` / `BcOp::CheckCap`; VM consults the installed
  gate at entry only when the flag is set. Zero overhead on scalar
  workloads, defense-in-depth on sensitive workloads.
* **C** — explicit `BytecodeEvaluator::with_capability_gate_strict()`
  host opt-in. Keeps default behaviour unchanged but every host has to
  opt-in twice (gate + strict), defeating the "real workloads trigger
  naturally" property.

**Picked B.** Default behaviour stays unchanged for the M2-A scalar
envelope; the activation fires the moment the bytecode compile pass
emits a `CallNative` / `CheckCap` (whether from a future widened
`lower_workspace_single` or from a hand-built BcFunction).

## Changes

* `BcFunction.requires_cap_consult: bool` (`op.rs`) — set by the
  compile pass, consulted at the VM entry. `Default` cleared.
* `compile::ops_contain_sensitive` + `compile_function_in_module`
  tail (`compile.rs`) — scans the emitted ops once, sets the flag if
  any `BcOp::CallNative | BcOp::CheckCap` is present.
* `CapabilityVtable::consult_all_declared_bits` (`vm.rs`) — sweeps
  every `CapabilityBit` variant through the gate; no-op when no gate
  is installed. Mirrors the cranelift backend's vtable-build sweep.
* `invoke_from_with_stack` entry sweep (`vm.rs`) — when
  `func.requires_cap_consult` is set, runs the new sweep after the
  existing `consult_all_granted_bits` step; first denial surfaces
  `BcVmError::CapabilityDenied`.
* 65 hand-built `BcFunction { .. }` test sites updated with
  `requires_cap_consult: false,` (scripted insertion).

LoC delta: roughly +90 logic, +110 test, +65 mechanical field-add.

## Tests Added

* `from_source_full_cap_gate_fires_on_sensitive_compiled_op` — hand-
  built BcFunction with a `CheckCap { NO_CAPABILITY_BIT }` op and the
  flag set; deny-all gate trips `CapabilityDenied` with the first
  declared bit (`ReadsFs`) at `steps == 0`, exactly one consult.
* `from_source_scalar_does_not_consult_gate_at_entry` — scalar
  `from_source` paired with a deny-all gate; asserts
  `ev.function().requires_cap_consult == false`, the run completes,
  and the gate records zero consults.

## Gate

* `cargo fmt --all --check`: clean
* `cargo clippy --workspace --all-targets -- -D warnings`: clean
* `cargo test --workspace`: 2258 passed, 0 failed, 6 ignored
* `cargo check --target wasm32-unknown-unknown -p relon-wasm`: clean
