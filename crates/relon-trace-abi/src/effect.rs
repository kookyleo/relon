//! Side-effect classification for trace ops.
//!
//! Shared ABI type. trace-jit / trace-emitter / codegen-native /
//! relon-ir all import this enum rather than redeclaring it. Phase
//! v6-γ M1 starts requiring every shared type live **only** in this
//! crate; the ABI smoke tests will reject any fork.
//!
//! ## Variant ordering & discriminants
//!
//! The variant order is load-bearing: it determines the integer
//! discriminant that the trace recorder serialises into golden trace
//! dumps. Reorder = ABI break. Add new variants at the **end** of the
//! list so existing discriminants stay stable.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// How a [`TraceOp`](https://docs.rs/relon-trace-jit) interacts with
/// state outside its SSA operands.
///
/// Variants are conservative: when in doubt, surface the **stricter**
/// class. A `Pure` op miscategorised as `Unrecoverable` only loses
/// optimisation opportunity; the reverse risks correctness.
///
/// ### Compatibility note
///
/// `relon_ir::EffectClass::UnrecoverableEffect` corresponds to
/// [`EffectClass::Unrecoverable`] here. The IR crate keeps the longer
/// name for backwards compatibility with v5-β-2 doc strings; phase
/// M1's migration step will collapse them into one source of truth
/// (this crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[repr(u8)]
pub enum EffectClass {
    /// No observable side effect. Inputs uniquely determine the
    /// output. Safe to inline / CSE / reorder freely across guards.
    Pure = 0,
    /// Reads external state without mutating it. Within a single
    /// trace the result is deterministic. May be reordered with other
    /// `Pure` / `ReadOnly` ops but **not** across a write op
    /// affecting the same location.
    ReadOnly = 1,
    /// Mutates state but the change is recoverable: the trace
    /// recorder snapshots the before-value into the guard's
    /// [`crate::DeoptStateSnapshot`] so a deopt can replay/restore
    /// it. Typical examples: bumping a scratch arena cursor,
    /// advancing an output list length.
    RecoverableWrite = 2,
    /// Mutates state in a way the trace recorder cannot undo (host
    /// call with hidden state, network IO, time-sensitive ops). The
    /// recorder **must** ABORT immediately when it observes one of
    /// these.
    Unrecoverable = 3,
}

impl EffectClass {
    /// Is the op safe to keep inside an in-flight trace?
    ///
    /// Returns `false` only for [`EffectClass::Unrecoverable`].
    pub fn is_traceable(self) -> bool {
        !matches!(self, EffectClass::Unrecoverable)
    }

    /// Does this op write state (recoverably or otherwise)?
    pub fn writes_state(self) -> bool {
        matches!(
            self,
            EffectClass::RecoverableWrite | EffectClass::Unrecoverable
        )
    }

    /// Does this op only read external state (no writes)?
    pub fn reads_state(self) -> bool {
        matches!(self, EffectClass::ReadOnly)
    }

    /// May the constant-folding pass legally fold an op of this class
    /// into a literal? Only `Pure` qualifies — read-only depends on
    /// external state and may differ across trace executions even
    /// when the *current* trace observed the same inputs.
    pub fn is_const_foldable(self) -> bool {
        matches!(self, EffectClass::Pure)
    }

    /// May the constant-folding / dead-store passes hop past this op
    /// when looking for a folding candidate further down the buffer?
    ///
    /// `RecoverableWrite` is a hard barrier: arena cursor moves and
    /// list-append slot bumps are order-sensitive, and we must not
    /// silently elide the side-effect chain by jumping past them.
    pub fn is_reorder_barrier(self) -> bool {
        matches!(
            self,
            EffectClass::RecoverableWrite | EffectClass::Unrecoverable
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_are_stable() {
        // Stable on-disk discriminant: golden trace dumps round-trip
        // through these integer values. If you change one, you must
        // ship a migration for every previously-recorded trace.
        assert_eq!(EffectClass::Pure as u8, 0);
        assert_eq!(EffectClass::ReadOnly as u8, 1);
        assert_eq!(EffectClass::RecoverableWrite as u8, 2);
        assert_eq!(EffectClass::Unrecoverable as u8, 3);
    }

    #[test]
    fn traceable_predicate() {
        assert!(EffectClass::Pure.is_traceable());
        assert!(EffectClass::ReadOnly.is_traceable());
        assert!(EffectClass::RecoverableWrite.is_traceable());
        assert!(!EffectClass::Unrecoverable.is_traceable());
    }

    #[test]
    fn write_and_read_predicates() {
        assert!(!EffectClass::Pure.writes_state());
        assert!(!EffectClass::ReadOnly.writes_state());
        assert!(EffectClass::RecoverableWrite.writes_state());
        assert!(EffectClass::Unrecoverable.writes_state());
        assert!(!EffectClass::Pure.reads_state());
        assert!(EffectClass::ReadOnly.reads_state());
    }

    #[test]
    fn fold_and_reorder_barriers() {
        assert!(EffectClass::Pure.is_const_foldable());
        assert!(!EffectClass::ReadOnly.is_const_foldable());
        assert!(!EffectClass::RecoverableWrite.is_const_foldable());
        assert!(!EffectClass::Unrecoverable.is_const_foldable());

        assert!(!EffectClass::Pure.is_reorder_barrier());
        assert!(!EffectClass::ReadOnly.is_reorder_barrier());
        assert!(EffectClass::RecoverableWrite.is_reorder_barrier());
        assert!(EffectClass::Unrecoverable.is_reorder_barrier());
    }
}
