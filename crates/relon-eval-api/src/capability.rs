//! Unified capability decision boundary across evaluator backends.
//!
//! Before this module, the tree-walker enforced capabilities at
//! dispatch time (`check_native_fn_capability` compared a
//! `NativeFnGate` against the `Context::capabilities` snapshot) while
//! the cranelift-native backend enforced them inside lowered IR (the
//! `cap_lookup` runtime helper returned a host-fn pointer from the
//! per-call `CapabilityVtable`; a null slot tripped a
//! `TrapKind::CapabilityDenied`). Both paths encoded the same policy
//! ("does the host grant the bit this native fn declared?") but
//! diverged on shape: one returned `Err(RuntimeError::CapabilityDenied)`,
//! the other returned `Err(RuntimeError::WasmCapabilityDenied)`, and
//! the audit surface was two unrelated source locations.
//!
//! [`CapabilityGate`] collapses both into a single
//! "is this capability granted?" decision the backends consult. The
//! tree-walker delegates `check_native_fn_capability` to the gate.
//! The cranelift backend's `CapabilityVtable::register_from_gate`
//! consults the gate at vtable-construction time so a denied bit is
//! materialised as a null slot, preserving the existing
//! null-pointer-traps-via-IR enforcement timing without duplicating
//! policy.
//!
//! Bytecode backend (`relon-bytecode::CapabilityVtable`) holds a
//! grant-tracking shape only; M2-A scaffold ships an empty vtable and
//! the host-fn dispatch path is M2-B work. Wiring it through this
//! trait lands together with M2-B and is documented in the
//! `relon-bytecode` crate root.
//!
//! # Enforcement-timing diff (kept intentional)
//!
//! * Tree-walker: `CapabilityGate::check` runs at *dispatch time* on
//!   every native-fn call site (cheap; we already touched the
//!   `NativeFnGate` to authenticate the call).
//! * Cranelift: `CapabilityGate::check` runs at *vtable-build time*
//!   (once per `run_main`); the in-IR `cap_lookup` then reads a
//!   pre-validated slot. A denied call still produces a
//!   `RuntimeError::WasmCapabilityDenied { cap_bit, range }` via the
//!   trap path so the host sees the same outcome class.
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
    /// Return `Ok(())` if the bit is granted; `Err(_)` otherwise.
    ///
    /// Implementations MAY classify the denial via
    /// [`DenyReason`] for the surrounding `RuntimeError` rendering.
    fn check(&self, cap: CapabilityBit) -> Result<(), CapabilityError>;

    /// Check every bit set on `gate`, short-circuit on the first
    /// denial. Returns `Ok(())` when the gate is fully satisfied —
    /// the canonical "may this native fn dispatch" question.
    ///
    /// The default impl walks the bits in `NativeFnGate::missing_bits`
    /// order so the failing bit matches the tree-walker's historical
    /// "first-missing" diagnostic shape. Implementations that want a
    /// different reporting order should override.
    fn check_gate(&self, gate: &NativeFnGate) -> Result<(), CapabilityError> {
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

/// Why a capability check failed. Carried alongside the bit so the
/// surrounding `RuntimeError` rendering can produce a human-readable
/// `reason` field without each backend re-deriving the string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The bit is simply not set in the active grant snapshot — the
    /// default-deny path the bare `Capabilities::default()` /
    /// `Context::sandboxed()` posture produces.
    NotGranted,
    /// Reserved for hosts that layer a trust-level (e.g. CLI
    /// `--trust=...` postures) on top of the bit grants. Default
    /// `Capabilities` impl never produces this; reserved for host-
    /// supplied gates that want to distinguish "the host policy
    /// rejected this bit" from "the bit was never granted".
    TrustLevelInsufficient,
    /// Reserved for sandbox-scoped denials where the bit is granted
    /// but a separate sandbox state (e.g. a still-loading module)
    /// disqualifies the call. Default impl never produces this.
    Sandbox,
    /// Free-form denial reason for host-supplied gates that need a
    /// specific message in the diagnostic.
    Other(String),
}

impl DenyReason {
    /// Short human-readable label for the diagnostic `reason` field.
    /// Backends embed this in their `CapabilityDenied { reason, ... }`
    /// payloads so the host's audit log sees a stable string.
    pub fn label(&self, cap: CapabilityBit) -> String {
        match self {
            DenyReason::NotGranted => {
                format!(
                    "function declared `{}` but caller did not grant it",
                    cap.as_str()
                )
            }
            DenyReason::TrustLevelInsufficient => {
                format!("host trust level insufficient for `{}`", cap.as_str())
            }
            DenyReason::Sandbox => {
                format!("sandbox state forbids `{}` at this call site", cap.as_str())
            }
            DenyReason::Other(msg) => msg.clone(),
        }
    }
}

/// Result of a denied [`CapabilityGate::check`]. Carries enough
/// information for either backend's surrounding
/// `RuntimeError` to render a diagnostic without re-deriving the
/// string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityError {
    /// The bit whose check failed.
    pub cap: CapabilityBit,
    /// Classification of the denial.
    pub reason: DenyReason,
}

impl CapabilityError {
    /// Convenience constructor for the dominant "bit not granted" case.
    pub fn not_granted(cap: CapabilityBit) -> Self {
        Self {
            cap,
            reason: DenyReason::NotGranted,
        }
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
}

/// Default gate implementation: consult the per-bit booleans on a
/// [`Capabilities`] snapshot.
///
/// `&Capabilities` is the natural carrier on the tree-walker path —
/// the `Context` already owns one. The cranelift backend constructs
/// its `CapabilityVtable` from this gate as well, so the two paths
/// share the exact same policy.
impl CapabilityGate for Capabilities {
    fn check(&self, cap: CapabilityBit) -> Result<(), CapabilityError> {
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
            Err(CapabilityError::not_granted(cap))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_deny_every_bit() {
        // The zero-trust default: every check returns NotGranted.
        let caps = Capabilities::default();
        for bit in [
            CapabilityBit::ReadsFs,
            CapabilityBit::WritesFs,
            CapabilityBit::Network,
            CapabilityBit::ReadsClock,
            CapabilityBit::ReadsEnv,
            CapabilityBit::UsesRng,
        ] {
            let err = caps.check(bit).expect_err("must deny");
            assert_eq!(err.cap, bit);
            assert_eq!(err.reason, DenyReason::NotGranted);
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
        let err = caps.check_gate(&gate).expect_err("must deny");
        assert_eq!(err.cap, CapabilityBit::ReadsFs);
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
    fn deny_reason_label_carries_capability_name() {
        let r = DenyReason::NotGranted.label(CapabilityBit::Network);
        assert!(r.contains("network"));
        let r =
            DenyReason::Other("sandboxed in audit mode".to_string()).label(CapabilityBit::ReadsFs);
        assert_eq!(r, "sandboxed in audit mode");
    }

    /// A host-supplied gate that always denies, to demonstrate the
    /// trust-level extension point.
    struct TrustGate;
    impl CapabilityGate for TrustGate {
        fn check(&self, cap: CapabilityBit) -> Result<(), CapabilityError> {
            Err(CapabilityError {
                cap,
                reason: DenyReason::TrustLevelInsufficient,
            })
        }
    }

    #[test]
    fn host_supplied_gate_can_override_with_trust_level() {
        let gate = NativeFnGate {
            reads_fs: true,
            ..NativeFnGate::default()
        };
        let err = TrustGate.check_gate(&gate).expect_err("must deny");
        assert_eq!(err.cap, CapabilityBit::ReadsFs);
        assert_eq!(err.reason, DenyReason::TrustLevelInsufficient);
    }
}
