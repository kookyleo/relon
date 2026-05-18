//! Deopt-state snapshot the cranelift emitter writes through the
//! `*mut TraceContext` arg on guard failure.
//!
//! Shared ABI types. trace-jit / trace-emitter / codegen-native all
//! import these definitions rather than redeclaring them. Phase v6-γ
//! M1 starts requiring every shared type live **only** in this crate;
//! any fork-definition will be rejected by ABI tests.
//!
//! ## Field semantics
//!
//! - `guard_pc` — `trace_pc` of the guard that fired. The host uses
//!   it to find the matching guard side-table entry that holds the
//!   `ssa_to_external_slot` mapping needed by
//!   [`DeoptStateSnapshot::apply`].
//! - `external_pc` — the resume instruction pointer (`*const u8`
//!   cast to `u64` for FFI portability). See
//!   [`crate::ExternalPc`] for the matching newtype.
//! - `ssa_slots_copy` — *snapshot* of `TraceContext::ssa_slots`
//!   taken at the instant the guard fired. Cloned (not aliased) so
//!   the dispatcher can free or reuse the original `ssa_slots` Box
//!   without invalidating the deopt info.
//! - `recoverable_writes` — drained out of
//!   `TraceContext::pending_recoverable_writes` in observed order;
//!   replayed by `apply` before the slot restoration.
//!
//! ## Apply ordering
//!
//! The convention:
//! 1. First replay every recoverable write (so memory that fused ops
//!    skipped is back to its pre-fusion state).
//! 2. Then write SSA slot values back into the generic frame's slots
//!    via the caller-supplied logic. The ABI crate stops at the
//!    "deopt state is materialised" point; the actual deopt
//!    dispatcher (host) decides how to push slot values back.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::context::TraceContext;

/// One recoverable write captured before a fused / dead-store-elim'd
/// store executed. On a guard failure we replay these in observed
/// order so the generic backend sees the same memory it would have
/// seen if no fusion happened.
///
/// `#[repr(C)]` is load-bearing: the cranelift emitter pushes records
/// onto `TraceContext::pending_recoverable_writes` by raw byte
/// offset.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct RecoverableWriteRecord {
    /// `*mut u8` cast to `u64`. Treated as an opaque address; the
    /// host knows how to interpret it (typically a scratch arena
    /// cursor or a list-append length slot).
    pub addr: u64,
    /// Value to restore at `addr`. Width-agnostic; the deopt path
    /// either widens narrower stores up to `u64` or relies on the
    /// host knowing the width per address.
    pub before_value: u64,
}

/// Populated by the trace runtime on guard failure; consumed by the
/// host's deopt dispatcher.
///
/// `#[repr(C)]` is load-bearing: the cranelift emitter writes
/// `guard_pc` and `external_pc` through this struct by raw byte
/// offset before tail-calling `__relon_trace_save_deopt` to populate
/// the heap-backed fields (`ssa_slots_copy`, `recoverable_writes`).
///
/// Reviewers: any field reorder/insert here **MUST** be matched in
/// `relon-trace-emitter`'s lowering and in the layout smoke tests.
#[repr(C)]
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DeoptStateSnapshot {
    /// `trace_pc` of the failed guard. Index into the trace's
    /// side-table list of guard sites.
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
    /// Convenience constructor used in tests; production code
    /// populates via the runtime helper
    /// `__relon_trace_save_deopt` (defined in `relon-trace-jit`).
    pub fn new(guard_pc: u32, external_pc: u64) -> Self {
        Self {
            guard_pc,
            external_pc,
            ssa_slots_copy: Vec::new().into_boxed_slice(),
            recoverable_writes: Vec::new(),
        }
    }

    /// Replay this snapshot onto the originating [`TraceContext`].
    ///
    /// Phase v6-γ M1 promotes [`DeoptStateSnapshot::apply`] from the
    /// trace-jit-private "apply into a `GenericState`" helper to a
    /// uniform "replay into the trace context the snapshot was taken
    /// from" operation. Both shapes are equivalent in effect — the
    /// snapshot owns `ssa_slots_copy` and `recoverable_writes`, and
    /// `apply` overwrites the live context with that owned data.
    ///
    /// Replay order:
    /// 1. Memory writes (so fused stores' pre-images are back in
    ///    place).
    /// 2. SSA slot values (so the host can read the guard-point
    ///    state out of the live context).
    ///
    /// # Safety
    ///
    /// `ctx` must be the same context the snapshot was taken from,
    /// or at least a context with the same slot count and an
    /// `ssa_slots` Box large enough to receive every entry in
    /// `ssa_slots_copy`. The runtime helper that populates the
    /// snapshot pins this invariant; callers passing in a foreign
    /// context risk truncated slot writes (silently — we skip
    /// out-of-range entries).
    ///
    /// `unsafe` because we re-publish observed memory writes through
    /// the host's `addr` u64s. The host is responsible for ensuring
    /// those addresses are still valid at deopt time.
    pub unsafe fn apply(&self, ctx: &mut TraceContext) {
        // Step 1: replay observed memory writes. The recorded `addr`
        // / `before_value` pairs are the bytes the trace would have
        // written had it not fused / DSE'd. Reapplying them puts the
        // world back where the generic backend would expect it on
        // resume.
        for w in &self.recoverable_writes {
            // SAFETY: the host pinned `addr` validity for the entire
            // trace lifetime. The cast from u64 -> *mut u64 widens
            // the address while keeping its bit pattern; the write
            // is u64-wide because every recoverable slot is stored
            // widened (matches `ExternalSlot::SLOT_WIDTH_BYTES`).
            unsafe {
                let ptr = w.addr as *mut u64;
                if !ptr.is_null() {
                    core::ptr::write_unaligned(ptr, w.before_value);
                }
            }
        }

        // Step 2: restore SSA slot values. The snapshot's
        // `ssa_slots_copy` is the authoritative pre-fusion view; we
        // overwrite the live ssa_slots with it. We bound by the
        // live ssa_slots length so a malformed snapshot can't push
        // past the allocation.
        let live = &mut ctx.ssa_slots;
        let n = live.len().min(self.ssa_slots_copy.len());
        live[..n].copy_from_slice(&self.ssa_slots_copy[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_new_default_fields() {
        let s = DeoptStateSnapshot::new(7, 0xfeed_face);
        assert_eq!(s.guard_pc, 7);
        assert_eq!(s.external_pc, 0xfeed_face);
        assert!(s.ssa_slots_copy.is_empty());
        assert!(s.recoverable_writes.is_empty());
    }

    #[test]
    fn recoverable_write_equality() {
        let a = RecoverableWriteRecord {
            addr: 0xaa,
            before_value: 1,
        };
        let b = RecoverableWriteRecord {
            addr: 0xaa,
            before_value: 1,
        };
        let c = RecoverableWriteRecord {
            addr: 0xbb,
            before_value: 1,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn apply_restores_ssa_slots() {
        let mut ctx = TraceContext::with_capacity(3);
        ctx.ssa_slots[0] = 0;
        ctx.ssa_slots[1] = 0;
        ctx.ssa_slots[2] = 0;
        let snap = DeoptStateSnapshot {
            guard_pc: 0,
            external_pc: 0,
            ssa_slots_copy: vec![11u64, 22, 33].into_boxed_slice(),
            recoverable_writes: vec![],
        };
        // SAFETY: empty recoverable_writes => no raw memory writes.
        unsafe {
            snap.apply(&mut ctx);
        }
        assert_eq!(&*ctx.ssa_slots, &[11u64, 22, 33]);
    }

    #[test]
    fn apply_replays_memory_then_slots() {
        // Use a stack buffer as the recoverable target so we can
        // observe the replay actually wrote it.
        let mut storage: u64 = 0;
        let addr = (&mut storage as *mut u64) as u64;

        let mut ctx = TraceContext::with_capacity(2);
        ctx.ssa_slots[0] = 0;
        ctx.ssa_slots[1] = 0;

        let snap = DeoptStateSnapshot {
            guard_pc: 1,
            external_pc: 0x2000,
            ssa_slots_copy: vec![99u64, 100].into_boxed_slice(),
            recoverable_writes: vec![RecoverableWriteRecord {
                addr,
                before_value: 0xdead_beef,
            }],
        };
        // SAFETY: `addr` points to `storage` which is live for the
        // duration of the test.
        unsafe {
            snap.apply(&mut ctx);
        }
        assert_eq!(storage, 0xdead_beef);
        assert_eq!(&*ctx.ssa_slots, &[99u64, 100]);
    }

    #[test]
    fn apply_clamps_oversize_snapshot() {
        // A malformed snapshot with more entries than the live ctx
        // must not panic / OOB — extra entries are silently
        // dropped, matching the trace-jit semantics.
        let mut ctx = TraceContext::with_capacity(2);
        let snap = DeoptStateSnapshot {
            guard_pc: 0,
            external_pc: 0,
            ssa_slots_copy: vec![1u64, 2, 3, 4, 5].into_boxed_slice(),
            recoverable_writes: vec![],
        };
        unsafe {
            snap.apply(&mut ctx);
        }
        assert_eq!(&*ctx.ssa_slots, &[1u64, 2]);
    }
}
