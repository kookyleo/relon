//! Effect classification of trace ops.
//!
//! Mirrors the `EffectClass` discussed in the v6-gamma design doc §3.
//!
//! - [`EffectClass::Pure`] — referentially transparent. Safe to inline,
//!   dedup, and reorder freely across guards.
//! - [`EffectClass::ReadOnly`] — reads external state but never mutates
//!   it. Within a single trace the result is deterministic, so the op
//!   may be hoisted across non-store ops.
//! - [`EffectClass::RecoverableWrite`] — mutates state but the
//!   pre-image is captured so a deopt can restore it (scratch arena
//!   cursor, list_append, etc.). The trace recorder must save the
//!   before-value into the enclosing [`crate::DeoptState`].
//! - [`EffectClass::Unrecoverable`] — irreversible external effect
//!   (network send, host function with hidden state). Trace JIT must
//!   **abort** the moment it sees an op with this class.
//!
//! Keeping this enum self-contained — i.e. _not_ a re-export of
//! `relon-ir`'s effect annotation — lets the trace-JIT prototype
//! evolve before the IR pinning is finalised, and gives us an obvious
//! integration point: a future `From<relon_ir::Effect>` impl.

use serde::{Deserialize, Serialize};

/// Side-effect classification used by trace recording and optimisation.
///
/// See the module-level docs for semantic rules each variant must obey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffectClass {
    /// No observable side effect. Inputs uniquely determine the
    /// output. Safe to inline / CSE / reorder across guards.
    Pure,
    /// Reads external state without mutating it. Within a single
    /// trace the result is deterministic. May be reordered with other
    /// `Pure` / `ReadOnly` ops but **not** across a write op affecting
    /// the same location.
    ReadOnly,
    /// Mutates state but the change is recoverable: the trace recorder
    /// snapshots the before-value into the guard's [`crate::DeoptState`]
    /// so a deopt can replay/restore it. Typical examples: bumping a
    /// scratch arena cursor, advancing an output list length.
    RecoverableWrite,
    /// Mutates state in a way the trace recorder cannot undo (host
    /// call with hidden state, network IO, time-sensitive ops). The
    /// recorder **must** ABORT immediately when it observes one of
    /// these.
    Unrecoverable,
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
    fn traceability_excludes_unrecoverable() {
        assert!(EffectClass::Pure.is_traceable());
        assert!(EffectClass::ReadOnly.is_traceable());
        assert!(EffectClass::RecoverableWrite.is_traceable());
        assert!(!EffectClass::Unrecoverable.is_traceable());
    }

    #[test]
    fn writes_state_matches_design() {
        assert!(!EffectClass::Pure.writes_state());
        assert!(!EffectClass::ReadOnly.writes_state());
        assert!(EffectClass::RecoverableWrite.writes_state());
        assert!(EffectClass::Unrecoverable.writes_state());
    }

    #[test]
    fn only_pure_is_const_foldable() {
        assert!(EffectClass::Pure.is_const_foldable());
        assert!(!EffectClass::ReadOnly.is_const_foldable());
        assert!(!EffectClass::RecoverableWrite.is_const_foldable());
        assert!(!EffectClass::Unrecoverable.is_const_foldable());
    }

    #[test]
    fn reorder_barrier_is_any_write() {
        assert!(!EffectClass::Pure.is_reorder_barrier());
        assert!(!EffectClass::ReadOnly.is_reorder_barrier());
        assert!(EffectClass::RecoverableWrite.is_reorder_barrier());
        assert!(EffectClass::Unrecoverable.is_reorder_barrier());
    }
}
