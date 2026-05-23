//! F-4 minimal prop-test skeleton: deopt-snapshot pc invariant.
//!
//! Target invariant (from `docs/internal/formalization-targets-2026-05-23.md`
//! §F-4):
//!
//! ```text
//! ∀ trace T, guard_pc g ∈ T.guards:
//!   let snap = deopt_snapshot(T, g) in
//!   ...
//!   ∧ snap.external_pc == g.external_pc
//! ```
//!
//! Full F-4 also requires `snap.ssa_stack ≡ tree_walker_eager_state` and
//! `snap.pending_writes ≡ tree_walker_recoverable_writes`. Those two
//! legs need a proptest-driven `TraceOp` generator plus a tree-walker
//! oracle and are deferred (3-5 day budget per the doc). This file
//! pins only the legs that do not require an oracle:
//!
//! 1. The `(guard_pc, external_pc)` pair the cranelift trace tail-calls
//!    `__relon_trace_save_deopt` with is faithfully echoed into the
//!    snapshot. Catches accidental field-order swaps in the helper or
//!    its emitter call site (today's emitter writes the pc fields by
//!    raw byte offset — a shuffle is silent unless this test fires).
//! 2. `ssa_slots_copy` is an *owned* clone of `TraceContext.ssa_slots`
//!    taken at the helper-call instant, and `recoverable_writes`
//!    captures `pending_recoverable_writes` in observed order while
//!    draining the source vector. These cover the "snapshot is a
//!    faithful checkpoint of context state, not an alias" invariant —
//!    the minimal precursor to the full tree-walker equivalence.
//!
//! See also: `runtime/deopt.rs` example-based unit tests, which pin the
//! same shape on hand-picked inputs; this module generalises them.

use proptest::collection::vec as prop_vec;
use proptest::prelude::*;

use super::deopt::__relon_trace_save_deopt;
use relon_trace_abi::TraceContext;

/// Bound on the SSA slot count and pending-write list length we
/// generate. Kept tight so each case is cheap and shrinker output stays
/// readable; the invariant is layout-level so wider inputs add no
/// signal.
const MAX_SLOTS: usize = 8;
const MAX_PENDING_WRITES: usize = 8;

proptest! {
    /// F-4 leg (3): `snap.external_pc == g.external_pc` and
    /// `snap.guard_pc == g.guard_pc`. The helper must echo both pc
    /// fields verbatim regardless of context shape.
    #[test]
    fn snapshot_echoes_guard_pc_and_external_pc(
        guard_pc in any::<u32>(),
        external_pc in any::<u64>(),
        slot_count in 0usize..=MAX_SLOTS,
        slot_values in prop_vec(any::<u64>(), 0..=MAX_SLOTS),
    ) {
        let mut ctx = TraceContext::with_capacity(slot_count);
        // Populate as many slots as the generated `slot_values` covers
        // (truncated by the smaller of the two lengths); leaves the
        // generator free to oversupply without skewing toward zero.
        let n = slot_count.min(slot_values.len());
        for (i, v) in slot_values.iter().take(n).enumerate() {
            ctx.ssa_slots[i] = *v;
        }
        // SAFETY: `&mut ctx as *mut _` is a valid, properly aligned,
        // exclusively-borrowed pointer that outlives the call.
        unsafe {
            __relon_trace_save_deopt(&mut ctx as *mut _, guard_pc, external_pc);
        }
        let snap = ctx.deopt_state.as_ref().expect("deopt_state populated");
        prop_assert_eq!(snap.guard_pc, guard_pc);
        prop_assert_eq!(snap.external_pc, external_pc);
    }

    /// F-4 precursor: `ssa_slots_copy` is a faithful owned clone of
    /// `TraceContext.ssa_slots` and `recoverable_writes` is the drained
    /// pending list in observed order. Both are required before the
    /// full tree-walker equivalence (§F-4 legs 1 + 2) can even be
    /// formulated.
    #[test]
    fn snapshot_clones_slots_and_drains_pending_writes(
        slot_values in prop_vec(any::<u64>(), 0..=MAX_SLOTS),
        pending in prop_vec((any::<u64>(), any::<u64>()), 0..=MAX_PENDING_WRITES),
        guard_pc in any::<u32>(),
        external_pc in any::<u64>(),
    ) {
        let mut ctx = TraceContext::with_capacity(slot_values.len());
        for (i, v) in slot_values.iter().enumerate() {
            ctx.ssa_slots[i] = *v;
        }
        for (addr, before) in &pending {
            ctx.record_pending_write(*addr, *before);
        }
        // SAFETY: see neighbouring test; identical contract.
        unsafe {
            __relon_trace_save_deopt(&mut ctx as *mut _, guard_pc, external_pc);
        }
        let snap = ctx.deopt_state.as_ref().expect("deopt_state populated");
        // Clone, not alias: the snapshot's slot copy matches what we
        // wrote, and the live ctx slots are untouched.
        prop_assert_eq!(&*snap.ssa_slots_copy, slot_values.as_slice());
        prop_assert_eq!(&*ctx.ssa_slots, slot_values.as_slice());
        // Drain: the pending list is consumed and the snapshot reflects
        // the original observed order.
        prop_assert!(ctx.pending_recoverable_writes.is_empty());
        prop_assert_eq!(snap.recoverable_writes.len(), pending.len());
        for (got, (addr, before)) in snap.recoverable_writes.iter().zip(pending.iter()) {
            prop_assert_eq!(got.addr, *addr);
            prop_assert_eq!(got.before_value, *before);
        }
    }
}
