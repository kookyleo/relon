//! `__relon_trace_save_deopt` + `DeoptStateSnapshot` runtime helpers.
//!
//! On a guard failure the cranelift-emitted trace tail calls
//! `__relon_trace_save_deopt(ctx, guard_pc, external_pc)` and then
//! returns `TraceEntryStatus::GuardFailed`. The host dispatcher,
//! seeing `GuardFailed`, reads `ctx->deopt_state` (populated by us
//! here) and applies it to the generic-backend's stack frame before
//! resuming execution at `external_pc`.
//!
//! ## Shared ABI
//!
//! v6-γ M1 promotes [`TraceContext`], [`DeoptStateSnapshot`] and
//! [`RecoverableWriteRecord`] to the shared `relon-trace-abi` crate.
//! This module re-exports the canonical definitions and supplies the
//! cranelift-side runtime helper [`__relon_trace_save_deopt`] plus
//! the tests-only [`GenericState`] frame mock. Reviewers MUST NOT
//! redeclare these ABI types here — there is **one** layout.
//!
//! ## Apply ordering
//!
//! When the host applies a snapshot back into a generic frame (the
//! mock here, the cranelift-generic backend in production) the
//! convention is:
//! 1. Replay every recoverable write (so memory the trace fused away
//!    is back to its pre-fusion state).
//! 2. Restore SSA slot values into the generic frame's slots via the
//!    guard's `(SsaVar, ExternalSlot)` mapping table.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::trace_ir::{ExternalSlot, SsaVar};

pub use relon_trace_abi::{DeoptStateSnapshot, RecoverableWriteRecord, TraceContext};

/// Tiny in-memory model of the generic-backend frame the deopt path
/// restores into. Real codegen uses cranelift slots / stack frames;
/// this model gives unit tests a deterministic verification target.
#[derive(Debug, Default)]
pub struct GenericState {
    /// `slot_id -> value` map. Keyed on `ExternalSlot.0` widened to
    /// `u64` for storage compactness.
    pub slots: Vec<(u64, u64)>,
    /// `addr -> value` replay log. Keyed on the raw `u64` addr.
    pub memory_replays: Vec<(u64, u64)>,
}

impl GenericState {
    /// Fresh empty frame mock.
    pub fn new() -> Self {
        Self::default()
    }

    /// Write (or overwrite) the value at `slot`.
    pub fn write_slot(&mut self, slot: ExternalSlot, value: u64) {
        let key = slot.0 as u64;
        if let Some(entry) = self.slots.iter_mut().find(|(s, _)| *s == key) {
            entry.1 = value;
        } else {
            self.slots.push((key, value));
        }
    }

    /// Record a recoverable-write replay.
    pub fn replay_write(&mut self, addr: u64, before_value: u64) {
        self.memory_replays.push((addr, before_value));
    }

    /// Read `slot` if present.
    pub fn slot(&self, slot: ExternalSlot) -> Option<u64> {
        let key = slot.0 as u64;
        self.slots.iter().find(|(s, _)| *s == key).map(|(_, v)| *v)
    }

    /// Apply a [`DeoptStateSnapshot`] into this generic frame via the
    /// supplied `(SsaVar, ExternalSlot)` mapping table.
    ///
    /// Mirrors the deopt protocol the cranelift-generic backend will
    /// follow in production: memory replays first, slot writes second.
    /// `ssa_slots_copy` is indexed by `SsaVar::raw()`; out-of-range
    /// mappings are silently skipped so a stale guard site cannot
    /// poison the test path.
    pub fn apply_snapshot(
        &mut self,
        snap: &DeoptStateSnapshot,
        slot_mappings: &[(SsaVar, ExternalSlot)],
    ) {
        for w in &snap.recoverable_writes {
            self.replay_write(w.addr, w.before_value);
        }
        for (ssa, ext_slot) in slot_mappings {
            let idx = ssa.raw() as usize;
            if let Some(&val) = snap.ssa_slots_copy.get(idx) {
                self.write_slot(*ext_slot, val);
            }
        }
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
    //
    // v6-δ M2-B: `value_stack_copy` is empty here — the trace JIT
    // operand model is purely SSA, so there is no per-step operand
    // stack to drain at guard-fire time. Mid-expression operand stack
    // rehydration happens host-side: backends that maintain an
    // operand-stack-aware resume index (today only the bytecode VM)
    // synthesise the value stack from the SSA snapshot + compile-time
    // "stack-recipe" metadata at resume entry.
    ctx.deopt_state = Some(DeoptStateSnapshot {
        guard_pc,
        external_pc,
        ssa_slots_copy,
        recoverable_writes,
        value_stack_copy: Vec::new().into_boxed_slice(),
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
            value_stack_copy: Vec::new().into_boxed_slice(),
        };
        let mappings = vec![
            (SsaVar(0), ExternalSlot(1000)),
            (SsaVar(2), ExternalSlot(1002)),
        ];
        let mut state = GenericState::new();
        state.apply_snapshot(&snap, &mappings);
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
            value_stack_copy: Vec::new().into_boxed_slice(),
        };
        // SsaVar(99) is out of range of the 2-slot copy.
        let mappings = vec![(SsaVar(99), ExternalSlot(0))];
        let mut state = GenericState::new();
        state.apply_snapshot(&snap, &mappings);
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
