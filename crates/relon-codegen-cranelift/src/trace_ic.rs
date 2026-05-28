//! v6-δ M2-C: Inline-cache dispatch slot for installed trace fns.
//!
//! ## Why
//!
//! Pre-M2-C the bench dispatches each iter via
//! `TraceJitState::lookup_trace(fn_id) -> Arc<JITedTraceFn>` ➜
//! `Arc::deref` ➜ `JITedTraceFn::invoke` ➜ `extern "C"` call ➜ status
//! enum match. Of the 9.52 ns/iter measured at v6-δ M1, roughly
//! 4.4 ns is invoke-path overhead unrelated to the trace body — the
//! trace itself only does two `load.i64`, one `sadd_overflow`, one
//! `brif` and one `store`.
//!
//! `TraceIcSlot` is the minimal scaffolding that lets a callsite skip
//! the lookup hop on every iter after the first:
//!
//! 1. On a hit (the cached `(fn_id, type_sig_hash)` matches) the slot
//!    yields a typed [`crate::trace_install::TraceEntryFn`] the caller
//!    can call directly — one indirect call, no `Arc` deref, no enum
//!    match, no `transmute` per iter.
//! 2. On a miss the slot consults the global `TraceJitState`,
//!    populates its cache, and returns the resolved entry. The cold
//!    path is shaped identical to today's bench warm-up — the
//!    `lookup_trace` cost is paid exactly once per slot insertion.
//!
//! ## Layout
//!
//! 4-way set-associative, each way carries:
//! - 64-bit type-signature hash (`fn_id` low bits + parameter types
//!   from the caller),
//! - typed entry pointer cast from the `JITedTraceFn`'s raw fn ptr,
//! - `Arc<JITedTraceFn>` (anchoring the JIT module's lifetime so the
//!   pointer stays callable as long as the slot holds the Arc).
//!
//! Total per-slot ≈ 4 × (8 + 8 + 16) = 128 bytes — sits in a single
//! L1 cache line pair, hot path is a single comparison + indirect
//! call.
//!
//! ## LRU policy
//!
//! Identical to [`relon_trace_jit::inline_cache::InlineCache`]: slot 0
//! is MRU, on hit promote, on miss evict the LRU slot (or the first
//! empty one from the tail). Thread-local storage keeps the cache
//! lock-free.
//!
//! ## Not the production IC
//!
//! Per the M2-C delivery brief, this is the "demonstration of IC
//! ceiling" — a thread-local hash-keyed slot rather than the full
//! cranelift-AOT-emitted call-site stub. The architecture differs in
//! three places:
//! - The slot lives in a Rust `thread_local!` rather than embedded
//!   inline next to a cranelift call instruction. Lookups still cost
//!   a function call (vs `cmp + je`) but the **dispatch tail** —
//!   `call rax` against the cached pointer — is identical.
//! - The miss path is Rust code (`global_trace_jit_state().lookup_trace`)
//!   rather than a fall-through `call ic_miss_handler` cranelift
//!   stub.
//! - Type-signature hashing is host-derived (we hash whatever the
//!   caller passes), not computed by cranelift from the call's arg
//!   types. The hash is opaque to the cache — equality is the only
//!   check.
//!
//! These differences don't move the bench number — the steady-state
//! cost is the indirect `call rax` plus its dependent load, plus the
//! trace body, plus the entry's epilogue/prologue. The wider design
//! (inline cranelift stubs) is the production target, scheduled for
//! a follow-up that has to touch `cranelift_module` patch points.

use std::cell::Cell;
use std::sync::Arc;

use crate::trace_install::{global_trace_jit_state, JITedTraceFn, TraceEntryFn};

/// Number of cache ways per IC slot. 4 is a textbook "megamorphic
/// tolerable" size: it covers monomorphic / bimorphic / mildly
/// polymorphic callsites cheaply, and pathological callers with > 4
/// distinct shapes hit the slow path anyway so the cache stops
/// helping.
pub const IC_WAYS: usize = 4;

/// One IC way: cached `(type_sig, entry, anchor)` triple. `Arc` keeps
/// the JIT module alive while the typed pointer remains in the slot.
///
/// `Clone` is derived so callers that need to inspect ways (e.g. for
/// telemetry) can clone the array out cheaply; the cost is one
/// atomic Arc bump per way.
#[derive(Clone)]
struct IcWay {
    type_sig: u64,
    entry: TraceEntryFn,
    /// Anchoring `Arc` keeps the JIT module alive for as long as
    /// the `entry` pointer might be dereferenced. Held but never
    /// read inside this struct — dead-code-analysis warning is
    /// silenced because of the Drop side effect (Arc decrement).
    #[allow(dead_code)]
    anchor: Arc<JITedTraceFn>,
}

/// Set-associative IC slot. Thread-local; safe to wrap in `Cell` so
/// the lookup path is allocation-free.
///
/// Use [`TraceIcSlot::lookup_or_install`] to dispatch — that's the
/// only public method. Internals stay private so future revisions
/// (e.g. swapping `Vec` to a fixed-size array) don't break callers.
pub struct TraceIcSlot {
    ways: Cell<[Option<IcWay>; IC_WAYS]>,
    hit_count: Cell<u64>,
    miss_count: Cell<u64>,
}

impl Default for TraceIcSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceIcSlot {
    /// Construct an empty IC slot. All ways start as `None`.
    pub fn new() -> Self {
        // Default-initialise the array via const block — `Option`
        // permits `[None; N]` only when the inner type is `Copy`; ours
        // is not. Build by hand.
        const NONE: Option<IcWay> = None;
        Self {
            ways: Cell::new([NONE; IC_WAYS]),
            hit_count: Cell::new(0),
            miss_count: Cell::new(0),
        }
    }

    /// Probe the cache for `(fn_id, type_sig)` and return the cached
    /// typed entry pointer on hit. On miss, consult
    /// [`global_trace_jit_state`] for the installed trace, populate
    /// the LRU way, and return the freshly-resolved pointer.
    ///
    /// Returns `None` if no trace is installed for `fn_id` — the
    /// caller's fallback path handles this just like the existing
    /// `TraceJitState::lookup_trace(fn_id) -> None` shape today.
    ///
    /// # Safety
    ///
    /// The returned `TraceEntryFn` is bound to the lifetime of the
    /// `JITedTraceFn` retained by this slot. The slot keeps the `Arc`
    /// alive across calls, so as long as the caller invokes the
    /// pointer **before** asking the slot for a different
    /// `(fn_id, type_sig)` that evicts this way, the pointer stays
    /// callable. In practice the hot loop pattern is "lookup once,
    /// call N times against the same fn_id" — eviction can't happen
    /// in that window.
    pub fn lookup_or_install(&self, fn_id: u32, type_sig: u64) -> Option<TraceEntryFn> {
        let mut ways = self.ways.take();
        let hit_idx = ways
            .iter()
            .position(|w| w.as_ref().is_some_and(|w| w.type_sig == type_sig));
        let result = match hit_idx {
            Some(0) => {
                // Already MRU.
                self.hit_count.set(self.hit_count.get() + 1);
                ways[0].as_ref().map(|w| w.entry)
            }
            Some(i) => {
                // Promote: shift 0..i down by one, place hit at 0.
                let promoted = ways[i].take();
                for j in (1..=i).rev() {
                    ways[j] = ways[j - 1].take();
                }
                let entry = promoted.as_ref().map(|w| w.entry);
                ways[0] = promoted;
                self.hit_count.set(self.hit_count.get() + 1);
                entry
            }
            None => {
                // Miss — resolve through the global registry.
                self.miss_count.set(self.miss_count.get() + 1);
                let state = global_trace_jit_state();
                let anchor = state.lookup_trace(fn_id)?;
                // SAFETY: anchor is held in the slot's way; the entry
                // pointer is valid for as long as `anchor` lives. We
                // store both atomically.
                let entry = unsafe { anchor.typed_entry() };
                // Evict LRU (or first empty from the tail) by shifting.
                let evict_at = ways
                    .iter()
                    .rposition(|w| w.is_none())
                    .unwrap_or(IC_WAYS - 1);
                for j in (1..=evict_at).rev() {
                    ways[j] = ways[j - 1].take();
                }
                ways[0] = Some(IcWay {
                    type_sig,
                    entry,
                    anchor,
                });
                Some(entry)
            }
        };
        self.ways.set(ways);
        result
    }

    /// Hit counter — used by tests + telemetry.
    pub fn hit_count(&self) -> u64 {
        self.hit_count.get()
    }

    /// Miss counter — used by tests + telemetry.
    pub fn miss_count(&self) -> u64 {
        self.miss_count.get()
    }

    /// Clear all ways. Used by hosts that invalidate a trace and want
    /// the next dispatch to re-resolve through the global registry.
    pub fn clear(&self) {
        const NONE: Option<IcWay> = None;
        self.ways.set([NONE; IC_WAYS]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_install::{RecordingRegistration, MAX_FN_ID};
    use relon_ir::ir::{Op, TaggedOp};
    use relon_parser::TokenRange;
    use relon_trace_abi::TraceContext;

    fn body_const_one() -> Vec<TaggedOp> {
        vec![
            TaggedOp {
                op: Op::ConstI64(1),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Return,
                range: TokenRange::default(),
            },
        ]
    }

    fn install_const_trace(fn_id: u32) {
        let _ = crate::trace_install::clear_recording(fn_id);
        crate::trace_install::register_recording(
            fn_id,
            RecordingRegistration {
                body: body_const_one(),
                param_tys: vec![],
                ..Default::default()
            },
        );
        let state = global_trace_jit_state();
        state.invalidate_trace(fn_id);
        // SAFETY: registered body has no LocalGet, so null args are
        // safe.
        unsafe {
            crate::trace_install::__relon_jump_to_recorder(fn_id, std::ptr::null());
        }
        assert!(state.lookup_trace(fn_id).is_some(), "trace must install");
    }

    #[test]
    fn lookup_hits_after_install_and_promotes_mru() {
        let fn_id = (MAX_FN_ID - 4) as u32;
        install_const_trace(fn_id);
        let slot = TraceIcSlot::new();
        // Cold miss.
        let entry_a = slot
            .lookup_or_install(fn_id, 0xa1)
            .expect("trace installed");
        assert_eq!(slot.miss_count(), 1);
        // Warm hit.
        let entry_b = slot.lookup_or_install(fn_id, 0xa1).expect("warm hit");
        assert_eq!(slot.hit_count(), 1);
        // Same pointer both times.
        assert_eq!(entry_a as usize, entry_b as usize);
        // Invocation through the cached pointer works.
        let mut ctx = TraceContext::with_capacity(8);
        // SAFETY: trace_const_one ignores its args.
        let status = unsafe { entry_b(&mut ctx as *mut _, std::ptr::null()) };
        assert_eq!(status, 0, "Success path");
        assert_eq!(ctx.result_slot, 1);
    }

    #[test]
    fn distinct_type_sigs_take_different_ways() {
        let fn_id = (MAX_FN_ID - 5) as u32;
        install_const_trace(fn_id);
        let slot = TraceIcSlot::new();
        for sig in 0..(IC_WAYS as u64) {
            let _ = slot.lookup_or_install(fn_id, sig).expect("trace installed");
        }
        assert_eq!(slot.miss_count() as usize, IC_WAYS);
        // Re-probe slot 0's sig: this was the FIRST miss, so it's now
        // at the back of the LRU after IC_WAYS-1 subsequent misses
        // pushed it down. It should still be present (4-way cache
        // holds 4 distinct sigs).
        assert!(slot.lookup_or_install(fn_id, 0).is_some());
        assert_eq!(slot.hit_count(), 1);
    }

    #[test]
    fn lookup_misses_when_no_trace_installed() {
        let fn_id = (MAX_FN_ID - 6) as u32;
        global_trace_jit_state().invalidate_trace(fn_id);
        let slot = TraceIcSlot::new();
        assert!(slot.lookup_or_install(fn_id, 0).is_none());
        assert_eq!(slot.miss_count(), 1);
    }
}
