//! Data structures the deopt path operates on.
//!
//! The full helper (`__relon_trace_save_deopt`) and the apply logic
//! land in subsequent commits. This commit introduces only the
//! structures so the rest of the runtime module can reference them.
//!
//! ## Why redeclare `TraceContext`?
//!
//! See the module-level docs on [`crate::runtime`]. TL;DR: the
//! emitter crate also defines a `TraceContext` (with a slightly
//! poorer `DeoptStateSnapshot`); we must not depend on that crate
//! from here, so we keep a layout-compatible view local.

use crate::trace_ir::ExternalSlot;

/// One recoverable write captured before a fused / dead-store-elim'd
/// store executed. On a guard failure we replay these in observed
/// order so the generic backend sees the same memory it would have
/// seen if no fusion happened.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableWriteRecord {
    /// `*mut u8` cast to `u64`. Treated as an opaque address; the
    /// host knows how to interpret it (typically a scratch arena
    /// cursor or a list-append length slot).
    pub addr: u64,
    /// Value to restore at `addr`. Width-agnostic; the deopt path
    /// either widens narrower stores up to u64 or relies on the host
    /// knowing the width per address.
    pub before_value: u64,
}

/// Populated by `__relon_trace_save_deopt` (next commit) and consumed
/// by the host's deopt dispatcher.
#[repr(C)]
#[derive(Debug)]
pub struct DeoptStateSnapshot {
    /// `trace_pc` of the failed guard. Index into the trace's
    /// side-table list of [`crate::GuardSite`].
    pub guard_pc: u32,
    /// Instruction pointer (cast to `u64`) the generic backend must
    /// resume at.
    pub external_pc: u64,
    /// Copy of `TraceContext.ssa_slots` at the deopt point. Owned by
    /// the snapshot; safe to outlive the originating context.
    pub ssa_slots_copy: Box<[u64]>,
    /// Recoverable writes drained out of the context's pending list,
    /// in the order they were recorded.
    pub recoverable_writes: Vec<RecoverableWriteRecord>,
}

impl DeoptStateSnapshot {
    /// Convenience constructor; production code populates via the
    /// `__relon_trace_save_deopt` helper landing in a follow-up commit.
    pub fn new(guard_pc: u32, external_pc: u64) -> Self {
        Self {
            guard_pc,
            external_pc,
            ssa_slots_copy: Vec::new().into_boxed_slice(),
            recoverable_writes: Vec::new(),
        }
    }
}

/// Layout-compatible view of `relon_trace_emitter::abi::TraceContext`.
///
/// **Field order is load-bearing**: the cranelift emitter reads /
/// writes these fields by byte offset. Any reorder here without a
/// matching change in the emitter crate **will** corrupt the deopt
/// path.
///
/// ## TODO (v6-gamma phase decision)
///
/// The emitter currently types `deopt_state` as `Option<EmitterSnapshot>`
/// where its snapshot lacks `ssa_slots_copy` / `recoverable_writes`.
/// The two definitions need to be reconciled (likely by promoting
/// this richer snapshot into a shared `relon-trace-abi` crate). Until
/// the reconcile lands the host **must** allocate `TraceContext`s
/// through this redeclared type when calling into a JIT-emitted
/// trace, otherwise the layouts won't match.
#[repr(C)]
pub struct TraceContext {
    /// Result slot the trace writes its return value into on success.
    pub result_slot: u64,
    /// One slot per SSA var the trace produced. The emitter writes
    /// here via the offset arithmetic in
    /// `ExternalSlotRepr::byte_offset` (= `index * 8`).
    pub ssa_slots: Box<[u64]>,
    /// Populated by `__relon_trace_save_deopt` on guard failure.
    /// `None` while the trace is mid-execution.
    pub deopt_state: Option<DeoptStateSnapshot>,
    /// Pending recoverable writes; populated by store fusion / DSE
    /// passes that emit `RecoverableWrite` ops at codegen time. The
    /// deopt path drains the whole vector into
    /// `deopt_state.recoverable_writes` and clears it.
    ///
    /// **Not present in the emitter's `TraceContext` today** — see
    /// the TODO above.
    pub pending_recoverable_writes: Vec<RecoverableWriteRecord>,
}

impl TraceContext {
    /// Allocate a context with `slot_count` SSA slots zeroed.
    pub fn with_capacity(slot_count: usize) -> Self {
        Self {
            result_slot: 0,
            ssa_slots: vec![0u64; slot_count].into_boxed_slice(),
            deopt_state: None,
            pending_recoverable_writes: Vec::new(),
        }
    }

    /// Push a recoverable-write record onto the pending list.
    pub fn record_pending_write(&mut self, addr: u64, before_value: u64) {
        self.pending_recoverable_writes
            .push(RecoverableWriteRecord { addr, before_value });
    }
}

/// Tiny in-memory model of the generic-backend frame the deopt path
/// restores into. Real codegen uses cranelift slots / stack frames;
/// this model gives unit tests a deterministic verification target.
#[derive(Debug, Default)]
pub struct GenericState {
    /// `slot_id -> value` map. Keyed on `ExternalSlot.0`.
    pub slots: Vec<(u64, u64)>,
    /// `addr -> value` replay log. Keyed on the raw `u64` addr.
    pub memory_replays: Vec<(u64, u64)>,
}

impl GenericState {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn write_slot(&mut self, slot: ExternalSlot, value: u64) {
        if let Some(entry) = self.slots.iter_mut().find(|(s, _)| *s == slot.0) {
            entry.1 = value;
        } else {
            self.slots.push((slot.0, value));
        }
    }
    pub fn replay_write(&mut self, addr: u64, before_value: u64) {
        self.memory_replays.push((addr, before_value));
    }
    pub fn slot(&self, slot: ExternalSlot) -> Option<u64> {
        self.slots
            .iter()
            .find(|(s, _)| *s == slot.0)
            .map(|(_, v)| *v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_context_initial_state() {
        let ctx = TraceContext::with_capacity(4);
        assert_eq!(ctx.result_slot, 0);
        assert_eq!(ctx.ssa_slots.len(), 4);
        assert!(ctx.deopt_state.is_none());
        assert!(ctx.pending_recoverable_writes.is_empty());
    }

    #[test]
    fn record_pending_write_appends() {
        let mut ctx = TraceContext::with_capacity(0);
        ctx.record_pending_write(0x100, 42);
        ctx.record_pending_write(0x200, 99);
        assert_eq!(ctx.pending_recoverable_writes.len(), 2);
        assert_eq!(ctx.pending_recoverable_writes[0].addr, 0x100);
        assert_eq!(ctx.pending_recoverable_writes[1].before_value, 99);
    }

    #[test]
    fn generic_state_slot_write_idempotent_on_same_id() {
        let mut state = GenericState::new();
        state.write_slot(ExternalSlot(7), 1);
        state.write_slot(ExternalSlot(7), 2);
        assert_eq!(state.slot(ExternalSlot(7)), Some(2));
        assert_eq!(state.slots.len(), 1);
    }
}
