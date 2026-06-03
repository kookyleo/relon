//! Side-effect classification for IR ops.
//!
//! Each [`crate::ir::Op`] variant maps to one [`EffectClass`] via
//! [`crate::ir::Op::effect_class`], letting const-folding / dead-store
//! passes decide what they may legally reorder, fold, or elide.
//!
//! ## Variant ordering & discriminants
//!
//! The variant order is load-bearing: it pins the integer discriminant
//! exposed via `#[repr(u8)]`. Reorder = ABI break. Add new variants at
//! the **end** of the list so existing discriminants stay stable.

/// How an [`Op`](crate::ir::Op) interacts with state outside its SSA
/// operands.
///
/// Variants are conservative: when in doubt, surface the **stricter**
/// class. A `Pure` op miscategorised as `Unrecoverable` only loses
/// optimisation opportunity; the reverse risks correctness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EffectClass {
    /// No observable side effect. Inputs uniquely determine the
    /// output. Safe to inline / CSE / reorder freely across guards.
    Pure = 0,
    /// Reads external state without mutating it. Within a single
    /// evaluation the result is deterministic. May be reordered with
    /// other `Pure` / `ReadOnly` ops but **not** across a write op
    /// affecting the same location.
    ReadOnly = 1,
    /// Mutates state but the change is recoverable: a before-value can
    /// be snapshotted so the effect can be replayed/restored. Typical
    /// examples: bumping a scratch arena cursor, advancing an output
    /// list length.
    RecoverableWrite = 2,
    /// Mutates state in a way that cannot be undone (host call with
    /// hidden state, network IO, time-sensitive ops).
    Unrecoverable = 3,
}

impl EffectClass {
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
    /// external state and may differ across executions even when the
    /// *current* run observed the same inputs.
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
        assert_eq!(EffectClass::Pure as u8, 0);
        assert_eq!(EffectClass::ReadOnly as u8, 1);
        assert_eq!(EffectClass::RecoverableWrite as u8, 2);
        assert_eq!(EffectClass::Unrecoverable as u8, 3);
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
