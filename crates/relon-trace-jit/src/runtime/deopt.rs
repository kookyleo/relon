//! `__relon_trace_save_deopt` + [`DeoptStateSnapshot`].
//!
//! On a guard failure the cranelift-emitted trace tail calls
//! `__relon_trace_save_deopt(ctx, guard_pc, external_pc)` and then
//! returns `TraceEntryStatus::GuardFailed`. The host dispatcher,
//! seeing `GuardFailed`, reads `ctx->deopt_state` (populated by us
//! here) and applies it to the generic-backend's stack frame before
//! resuming execution at `external_pc`.
//!
//! ## Field semantics
//!
//! - `guard_pc` — `trace_pc` of the guard that fired. The host uses
//!   it to find the matching [`GuardSite`](crate::GuardSite) in the
//!   trace's side tables and discover the `ssa_to_external_slot`
//!   mapping needed by [`DeoptStateSnapshot::apply`].
//! - `external_pc` — the `*const u8` instruction pointer (cast to
//!   `u64` for FFI portability) the generic backend must resume at.
//! - `ssa_slots_copy` — a *snapshot* of `ctx.ssa_slots` taken at the
//!   instant the guard fired. Cloned (not aliased) so the dispatcher
//!   can free or reuse the original `ctx.ssa_slots` Box without
//!   invalidating the deopt info.
//! - `recoverable_writes` — drained out of `ctx.pending_recoverable_writes`
//!   in observed order; replayed by `apply` before the slot
//!   restoration.
//!
//! ## Apply ordering
//!
//! The convention (mirroring [`crate::DeoptState::apply`]):
//! 1. First replay every recoverable write (so memory that fused ops
//!    skipped is back to its pre-fusion state).
//! 2. Then write SSA slot values back into the generic frame's slots
//!    via the caller-supplied `slot_mappings`.
//!
//! ## Why redeclare `TraceContext`?
//!
//! See the module-level docs on [`crate::runtime`]. TL;DR: the
//! emitter crate also defines a `TraceContext` (with a slightly
//! poorer `DeoptStateSnapshot`); we must not depend on that crate
//! from here, so we keep a layout-compatible view local. Reconciling
//! the two definitions is on the v6-gamma integration TODO list.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::trace_ir::{ExternalSlot, SsaVar};

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

/// Populated by `__relon_trace_save_deopt`; consumed by the host's
/// deopt dispatcher.
#[repr(C)]
#[derive(Debug)]
pub struct DeoptStateSnapshot {
    /// `trace_pc` of the failed guard. Index into the trace's
    /// side-table list of [`GuardSite`](crate::GuardSite).
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
    /// Convenience constructor used in tests; production code populates
    /// via [`__relon_trace_save_deopt`].
    pub fn new(guard_pc: u32, external_pc: u64) -> Self {
        Self {
            guard_pc,
            external_pc,
            ssa_slots_copy: Vec::new().into_boxed_slice(),
            recoverable_writes: Vec::new(),
        }
    }

    /// Replay this snapshot onto a caller-supplied generic-state
    /// view. `slot_mappings` carries the `(SsaVar, ExternalSlot)`
    /// pairs that the matching [`GuardSite`](crate::GuardSite) holds;
    /// callers typically zip the snapshot's `ssa_slots_copy` with
    /// `slot_mappings` by SSA index.
    ///
    /// Ordering:
    /// 1. Recoverable writes replayed first (memory back to pre-fusion).
    /// 2. SSA slots restored into the generic frame second.
    pub fn apply(&self, slot_mappings: &[(SsaVar, ExternalSlot)], state: &mut GenericState) {
        for w in &self.recoverable_writes {
            state.replay_write(w.addr, w.before_value);
        }
        for (ssa, ext_slot) in slot_mappings {
            // `ssa_slots_copy` is indexed by ssa-var raw id; out-of-range
            // mappings are silently skipped so a stale GuardSite cannot
            // poison the deopt path.
            let idx = ssa.raw() as usize;
            if let Some(&val) = self.ssa_slots_copy.get(idx) {
                state.write_slot(*ext_slot, val);
            }
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
    /// Populated by [`__relon_trace_save_deopt`] on guard failure.
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

    /// Push a recoverable-write record onto the pending list. The
    /// emitter will eventually emit cranelift IR that calls a host
    /// helper to do this; the standalone API is kept available for
    /// unit tests and host-side fallbacks.
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

/// Counts every call into [`__relon_trace_save_deopt`]; exposed only
/// for diagnostics + tests, never for ordering decisions.
static SAVE_DEOPT_CALLS: AtomicU64 = AtomicU64::new(0);

/// Returns the cumulative number of `__relon_trace_save_deopt` calls
/// since process start. Useful for harness assertions in tests.
pub fn save_deopt_call_count() -> u64 {
    SAVE_DEOPT_CALLS.load(Ordering::Relaxed)
}

/// Host-side runtime helper invoked by trace-emitter cranelift IR on
/// guard failure.
///
/// ## Contract
///
/// - `ctx_ptr` must point to a live [`TraceContext`] (layout-compatible
///   redeclared view; see module docs).
/// - `guard_pc` is the `trace_pc` of the guard that fired.
/// - `external_pc` is the resume IP cast to `u64`.
///
/// The helper:
/// 1. Clones `ctx->ssa_slots` into a fresh `Box<[u64]>`.
/// 2. Drains `ctx->pending_recoverable_writes` into a `Vec`.
/// 3. Wraps both in a [`DeoptStateSnapshot`] and stores it into
///    `ctx->deopt_state` (overwriting any prior snapshot).
///
/// ## Safety
///
/// `ctx_ptr` must be a valid, properly aligned, exclusively-borrowed
/// pointer to a [`TraceContext`]. The trace emitter guarantees this:
/// the entry signature pins arg 0 to `*mut TraceContext`, and only
/// one trace runs per thread context at a time (design doc §1.4).
#[no_mangle]
pub unsafe extern "C" fn __relon_trace_save_deopt(
    ctx_ptr: *mut TraceContext,
    guard_pc: u32,
    external_pc: u64,
) {
    SAVE_DEOPT_CALLS.fetch_add(1, Ordering::Relaxed);

    debug_assert!(
        !ctx_ptr.is_null(),
        "__relon_trace_save_deopt: ctx_ptr is null"
    );
    if ctx_ptr.is_null() {
        // In release builds, refuse to dereference null rather than
        // crash the host. The trace will simply fail to produce a
        // deopt snapshot; the host dispatcher must handle that.
        return;
    }

    let ctx: &mut TraceContext = &mut *ctx_ptr;

    // 1. Copy current ssa_slots into an owned Box. We deliberately
    //    clone rather than alias: the dispatcher may free the
    //    originating context immediately after reading deopt_state.
    let ssa_slots_copy: Box<[u64]> = ctx.ssa_slots.iter().copied().collect();

    // 2. Drain the pending recoverable writes. After drain, the
    //    context's pending list is empty; any future writes in the
    //    same trace will be observed afresh.
    let recoverable_writes: Vec<RecoverableWriteRecord> =
        std::mem::take(&mut ctx.pending_recoverable_writes);

    // 3. Compose the snapshot and stash it on the context.
    ctx.deopt_state = Some(DeoptStateSnapshot {
        guard_pc,
        external_pc,
        ssa_slots_copy,
        recoverable_writes,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    #[test]
    fn save_deopt_writes_snapshot_into_ctx() {
        let mut ctx = TraceContext::with_capacity(3);
        ctx.ssa_slots[0] = 11;
        ctx.ssa_slots[1] = 22;
        ctx.ssa_slots[2] = 33;
        unsafe {
            __relon_trace_save_deopt(&mut ctx as *mut _, 7, 0xdead_beef);
        }
        let snap = ctx.deopt_state.as_ref().expect("deopt_state populated");
        assert_eq!(snap.guard_pc, 7);
        assert_eq!(snap.external_pc, 0xdead_beef);
    }

    #[test]
    fn snapshot_ssa_slots_copy_matches_context() {
        let mut ctx = TraceContext::with_capacity(4);
        ctx.ssa_slots[0] = 100;
        ctx.ssa_slots[1] = 200;
        ctx.ssa_slots[2] = 300;
        ctx.ssa_slots[3] = 400;
        unsafe {
            __relon_trace_save_deopt(&mut ctx as *mut _, 1, 0x1000);
        }
        let snap = ctx.deopt_state.as_ref().unwrap();
        assert_eq!(&*snap.ssa_slots_copy, &[100u64, 200, 300, 400]);
        // Original ssa_slots untouched; the snapshot is an *owned* clone.
        assert_eq!(&*ctx.ssa_slots, &[100u64, 200, 300, 400]);
    }

    #[test]
    fn snapshot_drains_recoverable_writes() {
        let mut ctx = TraceContext::with_capacity(0);
        ctx.record_pending_write(0xaa, 1);
        ctx.record_pending_write(0xbb, 2);
        ctx.record_pending_write(0xcc, 3);
        unsafe {
            __relon_trace_save_deopt(&mut ctx as *mut _, 5, 0x2000);
        }
        let snap = ctx.deopt_state.as_ref().unwrap();
        assert_eq!(snap.recoverable_writes.len(), 3);
        assert_eq!(snap.recoverable_writes[0].addr, 0xaa);
        assert_eq!(snap.recoverable_writes[1].addr, 0xbb);
        assert_eq!(snap.recoverable_writes[2].addr, 0xcc);
        // After drain, the context's pending list is empty.
        assert!(ctx.pending_recoverable_writes.is_empty());
    }

    #[test]
    fn snapshot_records_guard_pc_and_external_pc() {
        let mut ctx = TraceContext::with_capacity(1);
        unsafe {
            __relon_trace_save_deopt(&mut ctx as *mut _, 42, 0xfeed_face);
        }
        let snap = ctx.deopt_state.as_ref().unwrap();
        assert_eq!(snap.guard_pc, 42);
        assert_eq!(snap.external_pc, 0xfeed_face);
    }

    #[test]
    fn snapshot_apply_restores_generic_state() {
        let snap = DeoptStateSnapshot {
            guard_pc: 9,
            external_pc: 0x4000,
            ssa_slots_copy: vec![10u64, 20, 30].into_boxed_slice(),
            recoverable_writes: vec![
                RecoverableWriteRecord {
                    addr: 0x100,
                    before_value: 0xabc,
                },
                RecoverableWriteRecord {
                    addr: 0x200,
                    before_value: 0xdef,
                },
            ],
        };
        let mappings = vec![
            (SsaVar(0), ExternalSlot(1000)),
            (SsaVar(2), ExternalSlot(1002)),
        ];
        let mut state = GenericState::new();
        snap.apply(&mappings, &mut state);
        // Memory replay happens first, slot writes second.
        assert_eq!(state.memory_replays, vec![(0x100, 0xabc), (0x200, 0xdef)]);
        assert_eq!(state.slot(ExternalSlot(1000)), Some(10));
        assert_eq!(state.slot(ExternalSlot(1002)), Some(30));
        // Slot for SSA(1) was never mapped, so generic frame still
        // has no entry for that slot id.
        assert_eq!(state.slot(ExternalSlot(1001)), None);
    }

    #[test]
    fn concurrent_trace_contexts_dont_interfere() {
        let num_threads = 8;
        let barrier = Arc::new(Barrier::new(num_threads));
        let mut handles = Vec::new();
        for tid in 0..num_threads {
            let b = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let mut ctx = TraceContext::with_capacity(2);
                ctx.ssa_slots[0] = tid as u64;
                ctx.ssa_slots[1] = (tid as u64) * 10;
                ctx.record_pending_write(tid as u64, tid as u64 + 1);
                b.wait();
                unsafe {
                    __relon_trace_save_deopt(&mut ctx as *mut _, tid as u32, tid as u64 + 0x1000);
                }
                let snap = ctx.deopt_state.unwrap();
                assert_eq!(snap.guard_pc, tid as u32);
                assert_eq!(snap.external_pc, tid as u64 + 0x1000);
                assert_eq!(snap.ssa_slots_copy[0], tid as u64);
                assert_eq!(snap.ssa_slots_copy[1], (tid as u64) * 10);
                assert_eq!(snap.recoverable_writes.len(), 1);
                assert_eq!(snap.recoverable_writes[0].addr, tid as u64);
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn snapshot_apply_skips_unmapped_ssa_vars() {
        let snap = DeoptStateSnapshot {
            guard_pc: 0,
            external_pc: 0,
            ssa_slots_copy: vec![7u64; 2].into_boxed_slice(),
            recoverable_writes: vec![],
        };
        // SsaVar(99) is out of range of the 2-slot copy.
        let mappings = vec![(SsaVar(99), ExternalSlot(0))];
        let mut state = GenericState::new();
        snap.apply(&mappings, &mut state);
        assert_eq!(state.slot(ExternalSlot(0)), None);
    }

    #[test]
    fn save_deopt_call_counter_increments() {
        let before = save_deopt_call_count();
        let mut ctx = TraceContext::with_capacity(0);
        unsafe {
            __relon_trace_save_deopt(&mut ctx as *mut _, 0, 0);
            __relon_trace_save_deopt(&mut ctx as *mut _, 0, 0);
        }
        let after = save_deopt_call_count();
        assert!(after >= before + 2);
    }
}
