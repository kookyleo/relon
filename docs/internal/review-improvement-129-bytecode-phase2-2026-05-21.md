# P1-B M2-B phase 2 stage report — bytecode CapabilityGate dispatch consult

Date: 2026-05-21
Base: b19f303 (phase 1 dormant hook landed)

## Audit of M2-A scaffold

The bytecode VM today dispatches only the scalar arith / cmp / control-flow / locals envelope. No `BcOp::CallNative`, no `BcOp::CheckCap`, no IO ops. The grant-bool table on `CapabilityVtable` is read by zero dispatch sites; the only capability-sensitive op is the static `BcOp::Trap(BcTrapKind::CapabilityDenied)`, and the IR-level `TrapKind` enum has no `CapabilityDenied` variant — so the standard `from_source` compile path never emits it. The trap fires only from hand-built `BcFunction`s (used by the partial-resume + sandbox prong tests).

Schema layer also carries no declared-capability metadata; `main_schema` only exposes field types.

## Consult strategy chosen

Two real enforcement points exist for phase 2:

1. **Dispatch-time pre-check** — `BytecodeVm::invoke_from_with_stack` now consults the installed gate over every grant-table bit before the first op tick. Denial trips `BcVmError::CapabilityDenied { cap_bit }` with the gate's failing bit; `steps` stays at 0 so callers can distinguish pre-dispatch denial from in-loop denial.
2. **Static-trap enrichment** — `BcOp::Trap(CapabilityDenied)` substitutes the legacy `u32::MAX` sentinel with `first_denied_bit(gate)` when a gate is installed. No gate → sentinel preserved (back-compat for the existing prong test).

Helper API `BytecodeVm::consult_capability_gate` + `CapabilityVtable::consult_gate` / `consult_all_granted_bits` exposed so phase 3 `BcOp::CallNative` / `BcOp::CheckCap` lowering can call into the same consult path without re-plumbing.

## Change set

| File | LoC | Note |
| --- | --- | --- |
| `crates/relon-bytecode/src/vm.rs` | +95 / -2 | `consult_gate` + `consult_all_granted_bits` + `consult_capability_gate` + dispatch pre-check + trap enrichment + `decode_cap_bit` / `first_denied_bit` helpers |
| `crates/relon-bytecode/src/evaluator.rs` | +15 / -7 | `with_capability_gate` doc rewrite to reflect phase 2 wiring |
| `crates/relon-bytecode/src/lib.rs` | +11 / -7 | crate-root doc covers phase 2 consult points |
| `crates/relon-bytecode/tests/bytecode_sandbox.rs` | +160 / -25 | dormant test upgraded to mechanism test + 4 new phase 2 tests |

## Tests added / upgraded

- `capability_gate_hook_can_be_installed_and_inspected` — scalar source with deny-all gate still completes (empty grant table → 0 consults). Phase 2 contract: scaffold envelope unchanged.
- `capability_gate_denial_surfaces_as_error_on_pre_dispatch_sweep` — grant `Network` + deny-all gate → `CapabilityDenied { cap_bit: 2 }` before any op runs (`steps == 0`).
- `capability_gate_grant_passes_pre_dispatch_sweep` — grant `ReadsClock` + allow-`ReadsClock` gate → run completes; 1 gate hit.
- `capability_trap_enrichment_uses_gate_bit_when_installed` — baseline `u32::MAX` sentinel preserved without a gate; with deny-all gate the trap reports `ReadsFs.bit_index() == 0`.
- `capability_gate_denial_lifts_to_runtime_error` — `BcVmError::into_runtime_error` lifts the enriched bit through `RuntimeError::WasmCapabilityDenied { cap_bit }`.

## Honesty note on scope

The task brief flagged the risk that phase 2 might lack a real enforcement point. Outcome: the pre-dispatch grant sweep + trap enrichment are real, tested enforcement, but they only activate when the host opts in (grants a bit, installs a gate, or hand-builds a BcFunction with the trap op). The standard `from_source` scalar path remains a no-op at runtime — by design, because there are no capability-sensitive ops to gate. Phase 3 IR coverage expansion (when `BcOp::CallNative` lands) will exercise the consult helpers per-call-site without further wire-up.

## Gate

- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace --tests --bins --examples` — 2032 passed, 0 failed (≥ 2029)
- `cargo check --target wasm32-unknown-unknown -p relon-bytecode` — clean
