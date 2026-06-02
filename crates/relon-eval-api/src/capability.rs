//! Unified capability decision boundary across evaluator backends.
//!
//! Every backend asks the same question before dispatching a guarded
//! native fn: *does the host grant the bit this fn declared?*
//! [`CapabilityGate`] is that single decision; the backends differ
//! only in *when* they consult it:
//!
//! * **Tree-walker** — at dispatch time, on every native-fn call site
//!   (`check_native_fn_capability` delegates straight to the gate).
//! * **Cranelift-native** — at vtable-build time (once per `run_main`):
//!   `CapabilityVtable::register_via_gate` consults the gate so a
//!   denied bit is materialised as a null slot, and the in-IR
//!   `cap_lookup` + null-check then traps on a denied call.
//! * **Bytecode VM** — at dispatch time, via the per-call-site
//!   `consult_gate` consult before any guarded op touches the stack.
//!
//! A denial surfaces as [`RuntimeError::CapabilityDenied`] across all
//! three (the compiled backends carry only the numeric `cap_bit`; the
//! tree-walker also fills a human-readable `reason`).
//!
//! [`RuntimeError::CapabilityDenied`]: crate::RuntimeError::CapabilityDenied
//!
//! Hosts that need a custom policy (e.g. trust-level thresholding,
//! per-call audit logging) implement [`CapabilityGate`] and wire it
//! anywhere the default `Capabilities`-driven gate is used today.

use crate::context::{Capabilities, CapabilityBit, NativeFnGate};

/// Single source of capability-policy truth for evaluator backends.
///
/// Implementations answer "is this capability bit granted for the
/// current evaluation context?". The default impl on
/// [`Capabilities`] reads the per-bit boolean fields; hosts can wrap
/// the default with auditing / trust-level layers by writing their
/// own impl.
///
/// The trait is intentionally minimal: one method, immutable
/// receiver, no async, no allocations. Backends must be able to call
/// this on hot paths (every native-fn dispatch for the tree-walker;
/// once per `run_main` for cranelift) without contention.
pub trait CapabilityGate: Send + Sync {
    /// Return `Ok(())` if the bit is granted; `Err(cap)` carrying the
    /// denied bit otherwise.
    fn check(&self, cap: CapabilityBit) -> Result<(), CapabilityBit>;

    /// Check every bit set on `gate`, short-circuit on the first
    /// denial. Returns `Ok(())` when the gate is fully satisfied —
    /// the canonical "may this native fn dispatch" question.
    ///
    /// The default impl walks the bits in `NativeFnGate::missing_bits`
    /// order so the failing bit matches the tree-walker's historical
    /// "first-missing" diagnostic shape. Implementations that want a
    /// different reporting order should override.
    fn check_gate(&self, gate: &NativeFnGate) -> Result<(), CapabilityBit> {
        if gate.reads_fs {
            self.check(CapabilityBit::ReadsFs)?;
        }
        if gate.writes_fs {
            self.check(CapabilityBit::WritesFs)?;
        }
        if gate.network {
            self.check(CapabilityBit::Network)?;
        }
        if gate.reads_clock {
            self.check(CapabilityBit::ReadsClock)?;
        }
        if gate.reads_env {
            self.check(CapabilityBit::ReadsEnv)?;
        }
        if gate.uses_rng {
            self.check(CapabilityBit::UsesRng)?;
        }
        Ok(())
    }
}

impl CapabilityBit {
    /// Stable, audit-visible string label for this capability.
    /// Mirrors the `NativeFnGate::missing_bits` field-name strings
    /// so historical diagnostics keep the same wording.
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityBit::ReadsFs => "reads_fs",
            CapabilityBit::WritesFs => "writes_fs",
            CapabilityBit::Network => "network",
            CapabilityBit::ReadsClock => "reads_clock",
            CapabilityBit::ReadsEnv => "reads_env",
            CapabilityBit::UsesRng => "uses_rng",
        }
    }

    /// Human-readable denial message for the `reason` field of
    /// [`RuntimeError::CapabilityDenied`]. The dominant (and only
    /// `Capabilities`-produced) case: the fn declared this bit but the
    /// caller never granted it.
    ///
    /// [`RuntimeError::CapabilityDenied`]: crate::RuntimeError::CapabilityDenied
    pub fn deny_message(self) -> String {
        format!(
            "function declared `{}` but caller did not grant it",
            self.as_str()
        )
    }
}

/// Default gate implementation: consult the per-bit booleans on a
/// [`Capabilities`] snapshot.
///
/// `&Capabilities` is the natural carrier on the tree-walker path —
/// the `Context` already owns one. The cranelift backend constructs
/// its `CapabilityVtable` from this gate as well, so the two paths
/// share the exact same policy.
impl CapabilityGate for Capabilities {
    fn check(&self, cap: CapabilityBit) -> Result<(), CapabilityBit> {
        let granted = match cap {
            CapabilityBit::ReadsFs => self.reads_fs,
            CapabilityBit::WritesFs => self.writes_fs,
            CapabilityBit::Network => self.network,
            CapabilityBit::ReadsClock => self.reads_clock,
            CapabilityBit::ReadsEnv => self.reads_env,
            CapabilityBit::UsesRng => self.uses_rng,
        };
        if granted {
            Ok(())
        } else {
            Err(cap)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_deny_every_bit() {
        // The zero-trust default: every check returns the denied bit.
        let caps = Capabilities::default();
        for bit in [
            CapabilityBit::ReadsFs,
            CapabilityBit::WritesFs,
            CapabilityBit::Network,
            CapabilityBit::ReadsClock,
            CapabilityBit::ReadsEnv,
            CapabilityBit::UsesRng,
        ] {
            let denied = caps.check(bit).expect_err("must deny");
            assert_eq!(denied, bit);
        }
    }

    #[test]
    fn all_granted_satisfies_every_bit() {
        let caps = Capabilities::all_granted();
        for bit in [
            CapabilityBit::ReadsFs,
            CapabilityBit::WritesFs,
            CapabilityBit::Network,
            CapabilityBit::ReadsClock,
            CapabilityBit::ReadsEnv,
            CapabilityBit::UsesRng,
        ] {
            caps.check(bit).expect("must grant");
        }
    }

    #[test]
    fn check_gate_short_circuits_on_first_missing_bit() {
        // Mirrors the tree-walker's historical "first-missing"
        // diagnostic: `reads_fs` is declared before `network` in the
        // field order, so it surfaces first.
        let caps = Capabilities::default();
        let gate = NativeFnGate {
            reads_fs: true,
            network: true,
            ..NativeFnGate::default()
        };
        let denied = caps.check_gate(&gate).expect_err("must deny");
        assert_eq!(denied, CapabilityBit::ReadsFs);
    }

    #[test]
    fn check_gate_passes_when_every_required_bit_granted() {
        let caps = Capabilities {
            reads_fs: true,
            network: true,
            ..Capabilities::default()
        };
        let gate = NativeFnGate {
            reads_fs: true,
            network: true,
            ..NativeFnGate::default()
        };
        caps.check_gate(&gate).expect("must allow");
    }

    #[test]
    fn pure_gate_passes_against_zero_grant() {
        // The pure-fn case: an all-zero gate is trivially satisfied
        // even by the fully-sandboxed default. This is the property
        // `register_pure_fn` relies on.
        let caps = Capabilities::default();
        let gate = NativeFnGate::default();
        caps.check_gate(&gate).expect("pure gate must always pass");
    }

    #[test]
    fn deny_message_carries_capability_name() {
        assert!(CapabilityBit::Network.deny_message().contains("network"));
        assert!(CapabilityBit::ReadsFs.deny_message().contains("reads_fs"));
    }

    /// A host-supplied gate that always denies, to demonstrate the
    /// custom-policy extension point.
    struct DenyAllGate;
    impl CapabilityGate for DenyAllGate {
        fn check(&self, cap: CapabilityBit) -> Result<(), CapabilityBit> {
            Err(cap)
        }
    }

    #[test]
    fn host_supplied_gate_can_override_policy() {
        let gate = NativeFnGate {
            reads_fs: true,
            ..NativeFnGate::default()
        };
        let denied = DenyAllGate.check_gate(&gate).expect_err("must deny");
        assert_eq!(denied, CapabilityBit::ReadsFs);
    }
}
