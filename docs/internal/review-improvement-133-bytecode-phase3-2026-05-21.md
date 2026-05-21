# Stage Report — Bytecode M2-B Phase 3 IR Coverage

Date: 2026-05-21
Worktree: `.claude/worktrees/agent-aca55595be585756c`
Branch: `worktree-agent-aca55595be585756c`
Base: `c55bfb3501d6bc174306f69ef35cc5f0ee6f4361` (local main, no fetch)

## Scope

M2-B phase 3 — IR coverage expansion for the bytecode VM. Phase 2
delivered the `CapabilityGate` consult helpers but had no IR-level ops
to drive them per call site; phase 3 lands the missing op shapes and
wires them through the existing helpers.

## `BcOp` table — before / after

Before (M2-A scaffold + M2-B phase 1 / 2): `ConstI64`, `ConstI32`,
`LocalGet`, `LocalSet`, `Add`/`Sub`/`Mul`/`Div`/`Mod`,
`Eq`/`Ne`/`Lt`/`Le`/`Gt`/`Ge`, `Jump` / `JumpIfTrue` / `JumpIfFalse`,
`Return`, `Trap(BcTrapKind)` — 20 variants.

After (M2-B phase 3): 24 variants. New entries:

- `BcOp::CallNative { import_idx, arg_count, cap_bit, ret_ty }` — IR
  `Op::CallNative` lower target; per-call-site capability consult.
- `BcOp::CheckCap { cap_bit }` — IR `Op::CheckCap` lower target;
  standalone capability consult; `NO_CAPABILITY_BIT` short-circuits.
- `BcOp::CallStdlibScalar { kind: BcStdlibKind, arg_count }` — scalar-
  pure stdlib handler dispatch for `IntAbs` / `IntMin` / `IntMax`.
- `BcOp::ListLen` — witness slot against constant-folded list-length
  representation; partial-resume keeps the slot for op-stream parity.

New supporting types: `BcStdlibKind` enum (`IntAbs` / `IntMin` /
`IntMax`) with `arity()` helper; `BcVmError::NativeNotImplemented
{ import_idx }` envelope lifting to `RuntimeError::Unsupported`.

## Capability consult triggers

Two new per-call-site consult sites on top of phase 2's pre-dispatch
sweep + trap-enrichment:

1. **`BcOp::CallNative` dispatch path** — consults `consult_gate`
   first; if no gate is installed the legacy grant table enforces
   the bit. `NO_CAPABILITY_BIT` skips the consult entirely (matches
   the cranelift `check_cap`-elision discipline).
2. **`BcOp::CheckCap` dispatch path** — same consult shape, no stack
   effect, no host-fn lookup. Sentinel bit is a no-op.

`BcOp::CallNative` drains its `arg_count` operands even on the
`NativeNotImplemented` trap path so the stack-recipe table stays
valid for partial-resume.

## Tests verified

`crates/relon-bytecode/tests/bytecode_sandbox.rs` grows by 10 phase-3
tests:

- `call_native_denied_by_gate_traps_with_declared_bit` — gate + grant
  fallback both surface the declared `cap_bit`.
- `call_native_passes_gate_but_traps_native_not_implemented` —
  capability prong passes, dispatcher reports `import_idx` round-trip.
- `call_native_no_capability_bit_skips_gate_consult` — sentinel skip;
  counts gate hits at 0 across the call.
- `check_cap_traps_when_bit_denied` / `check_cap_no_capability_bit_is_noop`.
- `call_stdlib_scalar_int_abs` / `call_stdlib_scalar_int_min_max` —
  pure i64 handler arithmetic.
- `list_len_witness_passes_length_through` — recipe stability.
- `call_native_lifts_to_unsupported_runtime_error` — envelope contract.

`cargo test --workspace` aggregate: **2047 passed** (≥ 2038 baseline).
`cargo clippy --workspace --all-targets -- -D warnings` clean.
`cargo fmt --all --check` clean. `cargo check -p relon-bytecode
--target wasm32-unknown-unknown` clean.

## Phase 4 blueprint (deferred)

- Host-fn registry on `CapabilityVtable` (`Vec<Option<Arc<dyn
  NativeHostFn>>>` indexed by `cap_bit` or import slot). Unfreezes the
  `NativeNotImplemented` trap and replaces step 3 of the
  `BcOp::CallNative` dispatcher.
- Real list / dict ops: `BcOp::ListGet`, `BcOp::ListLenReal`,
  `BcOp::DictLookup`, `BcOp::StrConcat`, `BcOp::StrContains` — all
  require a list / dict memory model (buffer-protocol arena or
  reference-counted slots).
- Trace-JIT hot counter + threshold trip + recorder bridge (the
  original RFC phase-3 deliverable; deferred so IR coverage could land
  first).
- 4-way bench row activation in `cranelift_aot_vs_tree_walk`.

## LoC delta

```
crates/relon-bytecode/src/compile.rs            +69
crates/relon-bytecode/src/lib.rs                +13
crates/relon-bytecode/src/op.rs                 +104
crates/relon-bytecode/src/vm.rs                 +120
crates/relon-bytecode/tests/bytecode_sandbox.rs +335
docs/internal/rfc-m2-b-... (status update)      +6 / -6
```

Net: ~641 inserted, ~14 reorganised.
