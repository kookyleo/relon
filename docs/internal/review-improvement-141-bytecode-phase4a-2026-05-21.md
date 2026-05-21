# M2-B phase 4a — bytecode host-fn registry (stage report)

Scope: ship the host-fn registry on `CapabilityVtable` so the
`BcOp::CallNative` dispatcher can invoke real host code on the
scalar lane. List / dict / string memory model stays deferred to
phase 4b.

## Registry design

- **Key**: `import_idx` (the slot `BcOp::CallNative` carries),
  not `cap_bit`. Multiple imports can share one cap bit; keying by
  import_idx keeps the registry unique-per-import and lets the
  per-call-site consult stay orthogonal to the lookup.
- **Storage**: `HashMap<u32, Arc<dyn RelonFunction>>` — `Arc` so
  hosts can share a fn across rebinds without cloning; trait object
  reuses `relon-eval-api`'s existing `RelonFunction` surface (no new
  trait needed, the tree-walker / cranelift backends already speak
  it).
- **Decode lane (scalar-only)**: phase 4a treats every popped slot
  as `Value::Int(slot as i64)`. The wider per-arg-type lane needs
  the phase-4b buffer-protocol memory model; tagging it now would
  duplicate work.
- **Encode lane**: `encode_value_for_ret(value, ret_ty, import_idx)`
  matches the cranelift backend's slot convention — Int/Bool/Null
  via i32/i64 lane, Float via `to_bits`. A non-scalar return surfaces
  as `HostFnReturnTypeMismatch` so the host gets a clear "route
  through cranelift/tree-walker" envelope rather than a silently-
  wrong slot.

## 改造点 + LoC

- `crates/relon-bytecode/src/vm.rs` (+231 / -28): `HashMap` import,
  `host_fns` field + `register_host_fn` / `resolve_host_fn` /
  `host_fn_count`, `BytecodeNativeFnCaps` stub (only `call_relon`
  needs an override — closures land later), `encode_value_for_ret`,
  two new `BcVmError` variants (`HostFnError`,
  `HostFnReturnTypeMismatch`) + their `into_runtime_error` lifts,
  and the `BcOp::CallNative` dispatch rewrite.
- `crates/relon-bytecode/tests/bytecode_sandbox.rs` (+389 / 0):
  seven new tests (registry dispatch + sum, gate denial skips host
  fn, unregistered slot fallback, bool return round-trip, host fn
  failure lift, arg-order pin, unsupported-return-type trap).
- `crates/relon-ir/src/stdlib/mod.rs` (+2 / -8): pre-existing fmt
  drift from main; folded in by `cargo fmt --all` on this branch.
- `docs/internal/rfc-m2-b-bytecode-jit-integration-2026-05-21.md`
  (+3 / -2): phase split updated to mark 4a landed and 4b explicit.

## Test verify

- `cargo test -p relon-bytecode`: 31 passed (24 baseline + 7 new),
  0 failed.
- `cargo test --workspace`: 2095 passed total (≥ 2047 phase-3
  baseline; growth from main + phase-4a tests).
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo fmt --all --check`: clean.
- `cargo check -p relon-bytecode --target wasm32-unknown-unknown`:
  clean.

Tests pin the four contract surfaces:

1. **Happy path** (`call_native_registry_dispatches_scalar_sum`,
   `call_native_registry_bool_return_round_trips`,
   `call_native_registry_arg_order_matches_declaration`).
2. **Gate denial precedes lookup**
   (`call_native_registry_gate_denial_skips_host_fn`).
3. **Unregistered slot keeps `NativeNotImplemented` bounce**
   (`call_native_unregistered_slot_keeps_native_not_implemented_fallback`).
4. **Error envelopes**
   (`call_native_host_fn_failure_lifts_to_unsupported`,
   `call_native_registry_unsupported_return_type_traps`).

## Out of scope (phase 4b)

- Per-arg-type decode lanes (String / List / Dict).
- Real `BcOp::ListGet` / `ListLenReal` / `DictLookup` / `StrConcat`
  etc. — needs the buffer-protocol memory model.
- `BytecodeNativeFnCaps::call_relon` real impl (requires bytecode VM
  frame stack — M3 work).
- Trace-JIT hot counter + recorder bridge.
- 4-way bench row activation.
