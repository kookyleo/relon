# P0-B Stage Report — CapabilityGate Unification

Date: 2026-05-21
Worktree: `.claude/worktrees/agent-afdf115082daecc07`
Branch: `main` (linear; no separate feature branch)

## Audit (before)

Two enforcement points existed independently:

1. Tree-walker — `relon-evaluator::eval.rs::check_native_fn_capability`
   compared `NativeFnGate` against `Context::capabilities` directly via
   `NativeFnGate::missing_bits`, raising `RuntimeError::CapabilityDenied
   { name, reason, range }` at *dispatch time*. Reason string was built
   ad-hoc inside the check.
2. Cranelift-native — `relon-codegen-native::sandbox.rs::CapabilityVtable`
   slot population was the policy decision; an empty slot tripped the
   IR-emitted null-check (`emit_check_cap` / `emit_call_native`) which
   raised `TrapKind::CapabilityDenied`, lifted to
   `RuntimeError::WasmCapabilityDenied { cap_bit, range }`.

P0-A confirmed: bytecode VM's `CapabilityVtable` is M2-A scaffold
(grant-tracking only, no host-fn dispatch yet) — explicitly excluded
from this round per the task's "no fake check" instruction.

Tree-walker AST walks `Expr::*` directly and never `dispatch`es through
the `OpVisitor`, so it does not participate in the cranelift policy
path; the two enforcement points are genuinely independent.

## CapabilityGate trait design

New module `relon-eval-api::capability` defines:

- `trait CapabilityGate: Send + Sync` with `check(cap: CapabilityBit) ->
  Result<(), CapabilityError>` plus a `check_gate(&NativeFnGate)`
  helper that walks each set bit in `NativeFnGate::missing_bits` order
  (preserves historical first-missing diagnostic shape).
- `enum DenyReason { NotGranted, TrustLevelInsufficient, Sandbox,
  Other(String) }` carrying classification for the surrounding
  `RuntimeError`. `DenyReason::label(cap)` produces the audit string.
- `struct CapabilityError { cap, reason }`.
- Blanket `impl CapabilityGate for Capabilities` consults the per-bit
  boolean fields — this is the default-deny path the bare
  `Capabilities::default()` produces.
- New `CapabilityBit::as_str()` helper for stable audit labels.

Re-exported at crate root as `CapabilityError`, `CapabilityGate`,
`DenyReason`.

## Backend wiring

- Tree-walker (`relon-evaluator::eval.rs::check_native_fn_capability`):
  delegates to `self.context.capabilities.check_gate(&entry.gate)`
  through the trait. Reason string now sourced from `DenyReason::label`.
  Error shape (`RuntimeError::CapabilityDenied { name, reason, range }`)
  preserved bit-for-bit so all 18 host-boundary `capability_*` tests
  remain green without modification.
- Cranelift (`relon-codegen-native::sandbox.rs`): new
  `CapabilityVtable::register_via_gate<G: CapabilityGate>(gate, cap,
  host_fn) -> bool` registers a slot only when `gate.check(cap)`
  passes; otherwise leaves the slot `None` so the existing
  `cap_lookup` null-check in IR raises `TrapKind::CapabilityDenied`
  unchanged. Module-level doc + `emit_check_cap` doc both crosslink to
  the trait to flag the shared policy surface. The IR shape (icmp_eq
  zero → cond_trap) is unchanged — this is intentional: enforcement
  *timing* differs (build-time for cranelift, dispatch-time for
  tree-walker), policy *source* is unified.
- Bytecode: untouched. `CapabilityVtable` is M2-A scaffold; M2-B will
  bridge through the same trait when host-fn dispatch lands.

## LoC delta

- New file `capability.rs`: +315 (incl. 7 unit tests).
- `relon-eval-api/src/lib.rs`: +7 / -0 (module + re-exports + doc).
- `relon-evaluator/src/eval.rs`: +12 / -7 (delegate to trait).
- `relon-codegen-native/src/sandbox.rs`: +58 / -0 (helper + 2 tests +
  module-level doc).
- `relon-codegen-native/src/codegen.rs`: +8 / -0 (doc crosslink).

Net: +400 / -7. 9 new unit tests (7 eval-api + 2 codegen-native).

## Security-audit surface improvement

Single grep target for capability policy: `CapabilityGate::check` in
`relon-eval-api::capability`. Both production enforcement paths now
route through this trait and the doc explicitly notes the timing diff
("dispatch-time for tree-walker, build-time for cranelift"). A host
auditor reviewing capability semantics reads exactly one trait + one
default impl, then follows the crosslinks to confirm both backends
honour it. Previously: two unrelated source locations encoded the same
policy; a host changing the rule (e.g. trust-level layering) had to
edit two files to keep them in lockstep, with no compiler help if they
drifted.

Hosts can now layer custom policy (e.g. CLI `--trust=...` overlays,
per-call audit logging) by implementing `CapabilityGate` and passing
their gate to `CapabilityVtable::register_via_gate` and any future
tree-walker gate hook — no fork of `Capabilities` field set required.

## Gate

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: 2022 tests pass, 0 failed.
- `cargo check --target wasm32-unknown-unknown` for eval-api /
  evaluator / wasm: clean.

## Blocked / partial

None. Bytecode wiring deferred to M2-B by design (no fake check).
