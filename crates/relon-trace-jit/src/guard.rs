//! Guard sites and the deopt-state structures they carry.
//!
//! A `GuardSite` is the contract between the optimised trace and the
//! generic cranelift code: if the predicate fails, the runtime must
//! restore enough state for the generic code to resume execution from
//! `deopt_pc` as if the trace had never run.
//!
//! Two pieces matter:
//! 1. `ssa_to_external_slot` -- map trace-local SSA values back to
//!    the generic code's value slots.
//! 2. `recoverable_writes` -- before-values captured for every
//!    `RecoverableWrite` op the optimiser may have fused or removed,
//!    so we can replay them when the guard fails.
//!
//! Both lists are owned by the [`DeoptState`] type, which exposes an
//! [`DeoptState::apply`] method usable in unit tests with a closure
//! representing the host write-back.

use serde::{Deserialize, Serialize};

use crate::trace_ir::{ExternalAddr, ExternalPc, ExternalSlot, GuardKind, SsaVar};

/// Restoration record for a single recoverable write. The trace
/// recorder fills `before_value` *before* the write executes; on a
/// guard failure we replay these writes (in original order) so the
/// generic code's view of memory matches what it would see if no
/// fusion had happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverableWrite {
    pub addr: ExternalAddr,
    pub before_value: u64,
}

/// Information needed to bail out of a trace at a specific guard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeoptState {
    /// Mapping of trace SSA ids to generic-code slots. Order is
    /// preserved so callers can index them positionally during
    /// codegen.
    pub ssa_to_external_slot: Vec<(SsaVar, ExternalSlot)>,
    /// Side effects that must be replayed on deopt. Stored in the
    /// order the recorder observed them.
    pub recoverable_writes: Vec<RecoverableWrite>,
}

impl DeoptState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a SSA -> external-slot binding. Idempotent: if the same
    /// SSA var is already bound the call is a no-op (last binding
    /// wins).
    pub fn bind(&mut self, ssa: SsaVar, slot: ExternalSlot) {
        if let Some(entry) = self
            .ssa_to_external_slot
            .iter_mut()
            .find(|(v, _)| *v == ssa)
        {
            entry.1 = slot;
        } else {
            self.ssa_to_external_slot.push((ssa, slot));
        }
    }

    /// Append a recoverable write to the replay list.
    pub fn record_recoverable_write(&mut self, write: RecoverableWrite) {
        self.recoverable_writes.push(write);
    }

    /// Apply this deopt state via a caller-supplied closure pair:
    /// `restore_slot(ssa, slot)` writes the SSA value back to the
    /// generic-code slot, and `replay_write(addr, before)` undoes a
    /// recoverable write. The closures are invoked in the recorded
    /// order. Used by unit tests in lieu of a real generic backend.
    pub fn apply<S, W>(&self, mut restore_slot: S, mut replay_write: W)
    where
        S: FnMut(SsaVar, ExternalSlot),
        W: FnMut(ExternalAddr, u64),
    {
        for (ssa, slot) in &self.ssa_to_external_slot {
            restore_slot(*ssa, *slot);
        }
        for w in &self.recoverable_writes {
            replay_write(w.addr, w.before_value);
        }
    }

    /// True if this deopt state binds anything (slot mappings or
    /// recoverable writes).
    pub fn is_empty(&self) -> bool {
        self.ssa_to_external_slot.is_empty() && self.recoverable_writes.is_empty()
    }
}

/// A guard inside a trace. The optimiser may add new sites during
/// type specialisation; existing sites must keep their `trace_pc`
/// stable so deopt dispatch keeps working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardSite {
    /// Index of the guard op in the trace's `ops` vector.
    pub trace_pc: u32,
    /// External PC the runtime must jump to if this guard fails.
    pub deopt_pc: ExternalPc,
    /// State required to make the deopt safe.
    pub deopt_state: DeoptState,
    /// Predicate kind (mirrors the op-level `GuardKind`).
    pub kind: GuardKind,
    /// v6-δ M2-C: snapshot of the IR walker's operand stack (in SSA-id
    /// form, oldest first / top last) **at the point this guard is
    /// emitted**. The bytecode-side resume path consumes the
    /// `value_stack_copy` Box derived from this snapshot (looked up
    /// through `ssa_slots_copy` at deopt time) to rehydrate the
    /// mid-expression operand stack rather than starting from the
    /// bc_idx's compile-time `stack_recipe` only.
    ///
    /// Carried out-of-band from `DeoptState` because:
    /// - `DeoptState::ssa_to_external_slot` is the (un-ordered) map of
    ///   "live SSAs at this guard" while this field is the ordered
    ///   stack view the resume API expects;
    /// - the optimiser may rewrite SSA ids without touching the stack
    ///   shape, so the two carry distinct optimiser-safety invariants.
    ///
    /// P2-16: `Box<[SsaVar]>` instead of `Vec<SsaVar>` so each stored
    /// guard pays one fewer machine word (no `cap` field) — meaningful
    /// when traces accumulate hundreds of guards. The slice is built
    /// once at emit time and never grown; the `Vec` capacity bookkeeping
    /// is pure overhead here. Serde wire format is unchanged —
    /// `Box<[T]>` round-trips as a sequence, identical to `Vec<T>`.
    #[serde(default)]
    pub ssa_stack_snapshot: Box<[SsaVar]>,
}

impl GuardSite {
    pub fn new(trace_pc: u32, deopt_pc: ExternalPc, kind: GuardKind) -> Self {
        Self {
            trace_pc,
            deopt_pc,
            deopt_state: DeoptState::new(),
            kind,
            ssa_stack_snapshot: Box::new([]),
        }
    }

    /// Builder-style attach the operand-stack snapshot (in SSA-id
    /// form). The recorder stamps this at emit-time from its
    /// `ssa_stack` mirror.
    pub fn with_ssa_stack_snapshot(mut self, snap: Box<[SsaVar]>) -> Self {
        self.ssa_stack_snapshot = snap;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_ir::ObservedType;

    #[test]
    fn bind_is_idempotent() {
        let mut s = DeoptState::new();
        s.bind(SsaVar(1), ExternalSlot(10));
        s.bind(SsaVar(1), ExternalSlot(20));
        assert_eq!(s.ssa_to_external_slot, vec![(SsaVar(1), ExternalSlot(20))]);
    }

    #[test]
    fn apply_calls_callbacks_in_order() {
        let mut s = DeoptState::new();
        s.bind(SsaVar(0), ExternalSlot(100));
        s.bind(SsaVar(1), ExternalSlot(101));
        s.record_recoverable_write(RecoverableWrite {
            addr: ExternalAddr(7),
            before_value: 42,
        });

        let mut slots = Vec::new();
        let mut writes = Vec::new();
        s.apply(
            |ssa, slot| slots.push((ssa, slot)),
            |addr, val| writes.push((addr, val)),
        );
        assert_eq!(
            slots,
            vec![
                (SsaVar(0), ExternalSlot(100)),
                (SsaVar(1), ExternalSlot(101)),
            ]
        );
        assert_eq!(writes, vec![(ExternalAddr(7), 42)]);
    }

    #[test]
    fn guardsite_starts_with_empty_state() {
        let gs = GuardSite::new(
            5,
            ExternalPc(0x1000),
            GuardKind::TypeCheck(SsaVar(2), ObservedType::I64),
        );
        assert!(gs.deopt_state.is_empty());
        assert_eq!(gs.trace_pc, 5);
    }
}
