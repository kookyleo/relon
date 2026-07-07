//! Sandbox primitives for the LLVM-native AOT (`emit_object`) backend.
//! **Phase C.**
//!
//! The cranelift-native backend (`relon-codegen-cranelift::sandbox`) is
//! the gold standard: it enforces the four hard sandbox guarantees the
//! wasm-AOT backend ships, expressed in cranelift IR. This module ports
//! the host-facing half of that surface to the LLVM backend so the
//! linked-after **native** binary produced by [`crate::LlvmAotEvaluator::emit_object`]
//! carries the same capability-gate + trap semantics.
//!
//! ## How the LLVM gate differs from the cranelift gate
//!
//! The two backends reach the *same* policy outcome
//! (`RuntimeError::CapabilityDenied` on a denied bit) through different
//! machinery:
//!
//! * **cranelift** parks an `extern "C"` host-fn pointer in a heap
//!   `CapabilityVtable`, takes its base address as a constant, and emits
//!   a per-call `cap_lookup` + null-check; a null slot traps.
//! * **LLVM** carries the host-granted capability set as the
//!   buffer-protocol entry's trailing `i64 caps` param (IR `LocalGet(4)`).
//!   `Op::CheckCap { cap_bit }` (lowered in `codegen/call.rs`) bakes a
//!   `(caps & (1 << cap_bit)) != 0` test into the emitted object; a clear
//!   bit records [`SandboxTrapKind::CapabilityDenied`] in
//!   `ArenaState::trap_code` and returns the negative sentinel so the host
//!   lifts a typed `RuntimeError` (rather than an `llvm.trap` `ud2` /
//!   SIGILL the host cannot catch on stable Rust).
//!
//! Because the gate is a *bitmask* on the LLVM side, the LLVM
//! [`CapabilityVtable`] is a thin builder around that `i64` mask: it
//! consults the same [`relon_eval_api::CapabilityGate`] policy the
//! cranelift backend and the tree-walker consult ([`Self::register_via_gate`])
//! and folds each granted bit into the mask the linked binary receives
//! as `caps`. The grant decision and the bit index are identical across
//! all three backends — only the runtime carrier differs.
//!
//! ## What lives where
//!
//! * [`SandboxConfig`] — compile-time knobs (mirror of cranelift's).
//! * [`SandboxTrapKind`] — the trap-cause enum, numbered to match
//!   cranelift's `TrapKind` and the [`crate::state::NativeTrap`] subset
//!   already recorded by the JIT-side dynamic dispatch helper.
//! * [`CapabilityVtable`] — the grant surface, expressed as an `i64`
//!   `caps` bitmask + the `import_idx`-keyed dynamic host-fn registry
//!   (which is just a re-export of [`crate::state::HostFnRegistry`], the
//!   existing LLVM equivalent of cranelift's `host_fns` half).
//!
//! `state.rs` (`ArenaState` / `HostFnRegistry` / `NativeTrap` /
//! `relon_llvm_call_native`) is consumed read-only by this module — it is
//! the codegen-visible runtime contract and must not change.

use relon_eval_api::{CapabilityBit, CapabilityGate, RelonFunction, RuntimeError};
use relon_parser::TokenRange;
use std::sync::Arc;

use crate::state::HostFnRegistry;

/// Compile-time sandbox configuration. Mirrors the cranelift backend's
/// `SandboxConfig` field-for-field so a side-by-side comparison of the
/// two AOT backends shares the same knob surface.
///
/// Production LLVM buffer entries emit the guard surface unconditionally:
/// arena bounds checks, div/mod guards, checked signed `Int` arithmetic,
/// capability gates, dynamic host-call trap lifting, and deterministic
/// step-budget fuel. This struct stays field-compatible with cranelift's
/// configuration so tests and host code can describe the same policy
/// intent across backends. The booleans are bench/debug intent records
/// for LLVM today; they should not be used to create a trusted execution
/// posture for untrusted source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    /// When `true`, host-visible memory access should be guarded
    /// against the arena byte length. LLVM buffer entries currently emit
    /// these guards unconditionally.
    pub bounds_check: bool,
    /// When `true`, resource exhaustion should be enforced. LLVM uses
    /// deterministic step-budget fuel configured on `LlvmAotEvaluator`
    /// rather than reading this bool directly as a wall-clock deadline
    /// switch.
    pub deadline_check: bool,
    /// When `true`, `Op::CheckCap` bakes the `caps`-bitmask test into
    /// the emitted object. The `codegen/call.rs` lowering already emits
    /// it unconditionally for a non-`NO_CAPABILITY_BIT` bit; this flag
    /// is the host-facing intent record.
    pub capability_check: bool,
    /// When `true`, `Op::Div` / `Op::Mod` emit an explicit divisor-zero
    /// guard before LLVM's `sdiv` / `srem` (whose div-by-zero is UB).
    /// The `codegen/arith.rs` lowering already emits it; this flag is
    /// the host-facing intent record.
    pub div_check: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            bounds_check: true,
            deadline_check: true,
            capability_check: true,
            div_check: true,
        }
    }
}

impl SandboxConfig {
    /// Disable all four guards. Bench-only — production code paths
    /// should never call this.
    pub fn unchecked() -> Self {
        Self {
            bounds_check: false,
            deadline_check: false,
            capability_check: false,
            div_check: false,
        }
    }
}

/// Trap kind raised by a guard inside LLVM-emitted native code. The
/// numeric values match the cranelift backend's `TrapKind` and the
/// `crate::state::NativeTrap` subset the JIT-side dynamic dispatch
/// helper already records, so the host decodes the same cause numbering
/// across backends. Encoded as `u64` so it fits the `ArenaState::trap_code`
/// slot the emitted object writes through `relon_llvm_call_native` /
/// the `Op::CheckCap` trap arm.
///
/// Only the subset the LLVM native path can currently raise
/// (`DivisionByZero` via the `sdiv`/`srem` guard, `BoundsViolation`
/// via arena guards, `CapabilityDenied` via `Op::CheckCap`,
/// `NumericOverflow` via checked Int arithmetic / reductions, and
/// `HostFnMissing`/`HostFnError` via dynamic dispatch) is reachable
/// today; the remaining variants are kept so the numbering stays a
/// faithful mirror of cranelift's for the deadline work that lands
/// with the wider emitter.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxTrapKind {
    /// Division (`Op::Div` / `Op::Mod`) by zero. Buffer entries record
    /// this code in `ArenaState::trap_code`; legacy/fast entries have no
    /// typed error lane and still use `llvm.trap`.
    DivisionByZero = 1,
    /// Pointer dereference walked past the arena bounds.
    BoundsViolation = 2,
    /// An `Op::CheckCap { cap_bit }` found the matching bit clear in the
    /// host-granted `caps` mask. Lifts to `RuntimeError::CapabilityDenied`.
    /// Matches cranelift's `TrapKind::CapabilityDenied` and
    /// `crate::state::NativeTrap::CapabilityDenied` (= 3).
    CapabilityDenied = 3,
    /// Per-call resource budget exhausted. LLVM currently raises this
    /// through deterministic step-budget fuel; a future wall-clock
    /// deadline can reuse the same trap code.
    ResourceExhausted = 4,
    /// No host fn registered at the requested `import_idx`, or no
    /// registry installed. Matches cranelift's `TrapKind::Unreachable`
    /// (= 5) and `crate::state::NativeTrap::HostFnMissing`; lifts to
    /// `RuntimeError::Unsupported`.
    HostFnMissing = 5,
    /// Signed integer overflow. Matches cranelift's
    /// `TrapKind::NumericOverflow` (= 6) and
    /// `crate::state::NativeTrap::NumericOverflow`. Raised by checked
    /// `Op::Add` / `Op::Sub` / `Op::Mul`, the `INT_MIN / -1` div/rem
    /// guard, and bundled checked reductions such as `list_int_sum`.
    NumericOverflow = 6,
    /// A host fn returned an error, or a value outside the scalar return
    /// envelope. Matches `crate::state::NativeTrap::HostFnError` (= 7);
    /// lifts to `RuntimeError::Unsupported`.
    HostFnError = 7,
}

impl SandboxTrapKind {
    /// Decode a `u64` recorded in `ArenaState::trap_code` back into a
    /// [`SandboxTrapKind`]. Unknown / `0` codes route to
    /// [`SandboxTrapKind::HostFnError`] so the host always gets a typed
    /// `RuntimeError` rather than a panic — matching cranelift's
    /// catch-all-into-typed-error posture.
    pub fn from_code(code: u64) -> SandboxTrapKind {
        match code {
            1 => SandboxTrapKind::DivisionByZero,
            2 => SandboxTrapKind::BoundsViolation,
            3 => SandboxTrapKind::CapabilityDenied,
            4 => SandboxTrapKind::ResourceExhausted,
            5 => SandboxTrapKind::HostFnMissing,
            6 => SandboxTrapKind::NumericOverflow,
            _ => SandboxTrapKind::HostFnError,
        }
    }

    /// Lift a trap kind into the appropriate [`RuntimeError`] variant.
    /// All trap mappings carry the entry function's source range so the
    /// diagnostic at least points at the `#main` declaration. Mirrors
    /// cranelift's `TrapKind::to_runtime_error` and the
    /// `crate::state::NativeTrap::runtime_error_from_code` subset.
    pub fn to_runtime_error(self, range: TokenRange) -> RuntimeError {
        match self {
            SandboxTrapKind::DivisionByZero => RuntimeError::DivisionByZero(range),
            SandboxTrapKind::BoundsViolation => RuntimeError::IndexOutOfBounds { range },
            SandboxTrapKind::CapabilityDenied => RuntimeError::CapabilityDenied {
                // The trap path carries no bit (the cleared mask bit is
                // the only signal), so the host gets a generic reason —
                // same posture as cranelift's null-slot trap.
                cap_bit: None,
                reason: "llvm-native: host-fn call denied by capability gate".to_string(),
                range,
            },
            SandboxTrapKind::ResourceExhausted => {
                RuntimeError::StepLimitExceeded { limit: None, range }
            }
            SandboxTrapKind::NumericOverflow => RuntimeError::NumericOverflow(range),
            SandboxTrapKind::HostFnMissing | SandboxTrapKind::HostFnError => {
                RuntimeError::Unsupported {
                    reason: "llvm-native: native-fn dispatch failed (host fn missing / errored / \
                             returned a non-scalar value)"
                        .to_string(),
                }
            }
        }
    }
}

/// Highest `cap_bit` the `i64 caps` bitmask can represent. Mirrors the
/// `cap_bit >= 64` guard `codegen/call.rs::emit_check_cap` enforces.
pub const MAX_CAP_BIT: u32 = 64;

/// The LLVM backend's capability grant surface.
///
/// On the cranelift side the equivalent `CapabilityVtable` is a heap
/// array of `extern "C"` host-fn pointers whose *non-null-ness* at
/// `slots[cap_bit]` is what lets an `Op::CheckCap { cap_bit }` pass. On
/// the LLVM side the granted set is carried as an `i64` bitmask the
/// buffer-protocol entry receives as its trailing `caps` param, so this
/// type is a thin builder around that mask plus the dynamic host-fn
/// registry the `import_idx`-keyed `Op::CallNative` dispatch resolves
/// against.
///
/// ## Two halves (same split as cranelift)
///
/// * `caps_mask` — the granted-capability bitmask. A set bit at index
///   `cap_bit` is what lets an `Op::CheckCap { cap_bit }` pass (the LLVM
///   analogue of cranelift's "non-null slot at `cap_bit`"). Built via
///   [`Self::grant`] / [`Self::register_via_gate`]; consumed by the host
///   as the `caps` word it hands to the linked entry (or to
///   `LlvmAotEvaluator::with_caps`).
/// * `host_fns` — the `import_idx`-keyed dynamic callable registry
///   ([`HostFnRegistry`]). A source-lowered
///   `Op::CallNative { cap_bit: NO_CAPABILITY_BIT }` resolves through it
///   via `relon_llvm_call_native`. Keyed off `import_idx` (a private
///   namespace) so it never collides with the `cap_bit`-indexed mask —
///   exactly cranelift's `host_fns` split.
#[derive(Default, Clone)]
pub struct CapabilityVtable {
    caps_mask: i64,
    host_fns: HostFnRegistry,
}

impl std::fmt::Debug for CapabilityVtable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityVtable")
            .field("caps_mask", &format_args!("{:#018b}", self.caps_mask))
            .field("host_fn_count", &self.host_fns.len())
            .finish()
    }
}

impl CapabilityVtable {
    /// Build an empty vtable: no capabilities granted, no host fns
    /// registered. The `n` argument is accepted for source-shape parity
    /// with cranelift's `with_capacity(n)`; the LLVM mask is fixed at 64
    /// bits so the value is only used to assert the caller does not ask
    /// for more bits than the `i64` mask can hold.
    pub fn with_capacity(_n: usize) -> Self {
        Self {
            caps_mask: 0,
            host_fns: HostFnRegistry::new(),
        }
    }

    /// Grant a capability bit by setting it in the `caps` mask. An
    /// `Op::CheckCap { cap_bit }` only tests the bit, so a set bit is
    /// enough to let the guard pass; the actual call dispatches through
    /// the `import_idx`-keyed `host_fns` registry. Mirrors cranelift's
    /// `CapabilityVtable::grant` (which parks a non-null sentinel).
    ///
    /// Bits `>= 64` are silently ignored (the `i64` mask cannot carry
    /// them); the matching `Op::CheckCap` lowering rejects an out-of-
    /// range bit at compile time, so a too-large grant can never satisfy
    /// a gate either way.
    pub fn grant(&mut self, cap_bit: u32) {
        if cap_bit < MAX_CAP_BIT {
            self.caps_mask |= 1i64 << cap_bit;
        }
    }

    /// Capability-gated grant. Consults `gate` for `cap_bit` via the
    /// shared [`relon_eval_api::CapabilityGate`] trait; if the gate
    /// denies the bit, the mask bit stays clear so the IR-level
    /// `Op::CheckCap` traps with [`SandboxTrapKind::CapabilityDenied`].
    /// This is the LLVM backend's half of the unified-enforcement
    /// design: the same policy the tree-walker consults at dispatch time
    /// and the cranelift backend consults at vtable-build time is
    /// consulted here when folding the bit into the `caps` mask, so
    /// denying a bit on the host side produces the same outcome class
    /// (`RuntimeError::CapabilityDenied`) on all three backends.
    ///
    /// Returns `true` if the bit was granted; `false` if the gate denied
    /// it (mask bit left clear).
    pub fn register_via_gate<G: CapabilityGate>(
        &mut self,
        gate: &G,
        cap_bit: CapabilityBit,
    ) -> bool {
        match gate.check(cap_bit) {
            Ok(()) => {
                self.grant(cap_bit.bit_index());
                true
            }
            Err(_) => false,
        }
    }

    /// `true` when `cap_bit` is granted in the mask. The LLVM analogue
    /// of cranelift's `lookup(cap_bit).is_some()`.
    pub fn is_granted(&self, cap_bit: u32) -> bool {
        cap_bit < MAX_CAP_BIT && (self.caps_mask & (1i64 << cap_bit)) != 0
    }

    /// The granted-capability bitmask, ready to hand to the linked
    /// entry as its trailing `caps` param (or to
    /// `LlvmAotEvaluator::with_caps`). This is the runtime carrier the
    /// `Op::CheckCap` gate baked into the emitted object reads.
    pub fn caps_mask(&self) -> i64 {
        self.caps_mask
    }

    /// Register a dynamic `Arc<dyn RelonFunction>` host fn at the given
    /// `import_idx`. Mirrors cranelift's
    /// `CapabilityVtable::register_host_fn`; delegates to the existing
    /// [`HostFnRegistry`] so the JIT-side `relon_llvm_call_native`
    /// dispatch resolves against the same map.
    pub fn register_host_fn(&mut self, import_idx: u32, func: Arc<dyn RelonFunction>) {
        self.host_fns.register(import_idx, func);
    }

    /// Resolve the dynamic host fn registered at `import_idx`. Mirrors
    /// cranelift's `CapabilityVtable::resolve_host_fn`.
    pub fn resolve_host_fn(&self, import_idx: u32) -> Option<&Arc<dyn RelonFunction>> {
        self.host_fns.resolve(import_idx)
    }

    /// Borrow the underlying [`HostFnRegistry`] so the evaluator can
    /// install it on a per-call `crate::state::ArenaState` via
    /// `ArenaState::install_host_fns`.
    pub fn host_fns(&self) -> &HostFnRegistry {
        &self.host_fns
    }

    /// Number of registered dynamic host fns.
    pub fn host_fn_count(&self) -> usize {
        self.host_fns.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_eval_api::{Capabilities, NativeArgs, Value};

    #[test]
    fn config_default_enables_all_guards() {
        let cfg = SandboxConfig::default();
        assert!(cfg.bounds_check);
        assert!(cfg.deadline_check);
        assert!(cfg.capability_check);
        assert!(cfg.div_check);
    }

    #[test]
    fn config_unchecked_disables_all_guards() {
        let cfg = SandboxConfig::unchecked();
        assert!(!cfg.bounds_check);
        assert!(!cfg.deadline_check);
        assert!(!cfg.capability_check);
        assert!(!cfg.div_check);
    }

    #[test]
    fn trap_kind_round_trips_through_u64_code() {
        for kind in [
            SandboxTrapKind::DivisionByZero,
            SandboxTrapKind::BoundsViolation,
            SandboxTrapKind::CapabilityDenied,
            SandboxTrapKind::ResourceExhausted,
            SandboxTrapKind::HostFnMissing,
            SandboxTrapKind::NumericOverflow,
            SandboxTrapKind::HostFnError,
        ] {
            let code = kind as u64;
            assert_eq!(SandboxTrapKind::from_code(code), kind);
        }
        // Unknown / 0 codes route to HostFnError (defensive catch-all).
        assert_eq!(SandboxTrapKind::from_code(0), SandboxTrapKind::HostFnError);
        assert_eq!(SandboxTrapKind::from_code(99), SandboxTrapKind::HostFnError);
    }

    #[test]
    fn trap_kind_numbering_mirrors_cranelift_and_native_trap() {
        // The numbering MUST match cranelift's TrapKind and the
        // crate-local NativeTrap subset so the host decodes one cause
        // numbering across backends.
        assert_eq!(SandboxTrapKind::CapabilityDenied as u64, 3);
        assert_eq!(SandboxTrapKind::HostFnMissing as u64, 5);
        assert_eq!(SandboxTrapKind::HostFnError as u64, 7);
        assert_eq!(
            SandboxTrapKind::CapabilityDenied as u64,
            crate::state::NativeTrap::CapabilityDenied as u64
        );
        assert_eq!(
            SandboxTrapKind::HostFnMissing as u64,
            crate::state::NativeTrap::HostFnMissing as u64
        );
        assert_eq!(
            SandboxTrapKind::HostFnError as u64,
            crate::state::NativeTrap::HostFnError as u64
        );
    }

    #[test]
    fn trap_kind_maps_to_runtime_error_variant() {
        let range = TokenRange::default();
        assert!(matches!(
            SandboxTrapKind::DivisionByZero.to_runtime_error(range),
            RuntimeError::DivisionByZero(_)
        ));
        assert!(matches!(
            SandboxTrapKind::BoundsViolation.to_runtime_error(range),
            RuntimeError::IndexOutOfBounds { .. }
        ));
        assert!(matches!(
            SandboxTrapKind::CapabilityDenied.to_runtime_error(range),
            RuntimeError::CapabilityDenied { .. }
        ));
        assert!(matches!(
            SandboxTrapKind::ResourceExhausted.to_runtime_error(range),
            RuntimeError::StepLimitExceeded { .. }
        ));
        assert!(matches!(
            SandboxTrapKind::NumericOverflow.to_runtime_error(range),
            RuntimeError::NumericOverflow(_)
        ));
        assert!(matches!(
            SandboxTrapKind::HostFnMissing.to_runtime_error(range),
            RuntimeError::Unsupported { .. }
        ));
    }

    #[test]
    fn grant_and_is_granted_round_trip() {
        let mut vt = CapabilityVtable::with_capacity(64);
        assert!(!vt.is_granted(2));
        vt.grant(2);
        assert!(vt.is_granted(2));
        assert!(!vt.is_granted(3));
        // The runtime carrier is the bitmask: bit 2 set.
        assert_eq!(vt.caps_mask(), 1i64 << 2);
    }

    #[test]
    fn grant_ignores_out_of_range_bits() {
        let mut vt = CapabilityVtable::with_capacity(64);
        vt.grant(64);
        vt.grant(200);
        assert_eq!(vt.caps_mask(), 0);
        assert!(!vt.is_granted(64));
    }

    #[test]
    fn register_via_gate_denies_when_capability_not_granted() {
        let caps = Capabilities::default();
        let mut vt = CapabilityVtable::with_capacity(64);
        // `reads_fs` not granted in the default snapshot — bit stays clear.
        let populated = vt.register_via_gate(&caps, CapabilityBit::ReadsFs);
        assert!(!populated, "denied gate must leave the mask bit clear");
        assert!(!vt.is_granted(CapabilityBit::ReadsFs.bit_index()));
        assert_eq!(vt.caps_mask(), 0);
    }

    #[test]
    fn register_via_gate_populates_when_capability_granted() {
        let caps = Capabilities::all_granted();
        let mut vt = CapabilityVtable::with_capacity(64);
        let populated = vt.register_via_gate(&caps, CapabilityBit::Network);
        assert!(populated, "granted gate must set the mask bit");
        assert!(vt.is_granted(CapabilityBit::Network.bit_index()));
        assert_eq!(vt.caps_mask(), 1i64 << CapabilityBit::Network.bit_index());
    }

    /// Mirrors cranelift's `host_fns` half: a registered callable is
    /// resolvable by `import_idx` and dispatch-callable.
    struct AddOne;
    impl RelonFunction for AddOne {
        fn call(&self, args: NativeArgs, _r: TokenRange) -> Result<Value, RuntimeError> {
            match args.positional.first() {
                Some(Value::Int(x)) => Ok(Value::Int(x + 1)),
                _ => Err(RuntimeError::Unsupported {
                    reason: "AddOne expects Int".into(),
                }),
            }
        }
    }

    #[test]
    fn host_fn_registry_round_trip() {
        let mut vt = CapabilityVtable::with_capacity(64);
        assert!(vt.resolve_host_fn(0).is_none());
        vt.register_host_fn(0, Arc::new(AddOne));
        assert_eq!(vt.host_fn_count(), 1);
        let f = vt.resolve_host_fn(0).expect("registered");
        let r = f
            .call(
                NativeArgs::from_positional(vec![Value::Int(41)], {
                    // reuse the crate's caps shim via a trivial closure-free path
                    use relon_eval_api::NativeFnCaps;
                    struct NoCb;
                    impl NativeFnCaps for NoCb {
                        fn call_relon(
                            &self,
                            _f: &Value,
                            _a: Vec<Value>,
                            _r: TokenRange,
                        ) -> Result<Value, RuntimeError> {
                            Err(RuntimeError::Unsupported {
                                reason: "no cb".into(),
                            })
                        }
                    }
                    Arc::new(NoCb)
                }),
                TokenRange::default(),
            )
            .expect("dispatch");
        assert_eq!(r, Value::Int(42));
    }
}
