//! `TraceContext` — the runtime state every cranelift-emitted trace
//! reads / writes through the `*mut TraceContext` arg.
//!
//! Shared ABI type. trace-jit / trace-emitter / codegen-native all
//! import this struct rather than redeclaring it. Phase v6-γ M1 starts
//! requiring every shared type live **only** in this crate; any
//! fork-definition will be rejected by ABI tests.
//!
//! ## Layout invariant
//!
//! Field order is **load-bearing**. The emitter encodes the byte
//! offsets of each field as cranelift constants at lowering time;
//! reordering breaks the deopt path, the result slot, and the
//! pending-write buffer simultaneously. Reviewers MUST update the
//! layout smoke tests (`tests/layout_smoke.rs`) in the same PR as any
//! intentional layout change.
//!
//! The canonical field order is:
//!
//! | Idx | Name                          | Type                                |
//! |-----|-------------------------------|-------------------------------------|
//! | 0   | `ssa_slots`                   | `Box<[u64]>` (16-byte fat pointer)  |
//! | 1   | `result_slot`                 | `u64`                               |
//! | 2   | `deopt_state`                 | `Option<DeoptStateSnapshot>`        |
//! | 3   | `host_hooks`                  | `HostHookTable`                     |
//! | 4   | `pending_recoverable_writes`  | `Vec<RecoverableWriteRecord>`       |
//! | 5   | `dict_lookup_ic`              | `[DictIcSlot; DICT_LOOKUP_IC_SLOT_COUNT]` |
//!
//! Putting `ssa_slots` first lets the emitter address it through a
//! zero-offset load of the context pointer — the most frequent
//! operation in the trace body. `result_slot` follows so its offset
//! (16 = sizeof(Box fat ptr)) is also constant-known at emit time.
//!
//! Idx 5 (`dict_lookup_ic`) was appended after the original 5-field
//! contract; emitter callers always resolve offsets through
//! `mem::offset_of!`, so re-ordering here only requires re-running
//! the layout smoke tests.

use crate::deopt::{DeoptStateSnapshot, RecoverableWriteRecord};

/// `extern "C"` function pointer type used by `on_trap` entries of
/// [`HostHookTable`].
///
/// The two-argument shape — `(*mut TraceContext, u32)` — is the
/// minimum surface for hooks that only need the context + a
/// classification id. `save_deopt` previously shared this shape with
/// a shim that dropped its `external_pc` arg; v6-δ M1 R5 introduces
/// the dedicated [`TraceSaveDeoptFn`] so the emitter can call the
/// helper without losing the resume PC.
pub type TraceHookFn = unsafe extern "C" fn(*mut TraceContext, u32);

/// `extern "C"` fn-pointer signature for the `save_deopt` slot.
///
/// Matches `__relon_trace_save_deopt(ctx, guard_pc, external_pc)`.
/// v6-δ M1 R5: the emitter dispatches through this typed slot via
/// `call_indirect`, threading the real `external_pc` through to the
/// host without the lossy void-return shim the v6-γ HostHookTable
/// used to wire here.
pub type TraceSaveDeoptFn = unsafe extern "C" fn(*mut TraceContext, u32, u64);

/// `extern "C"` fn-pointer signature for the `resolve_call` slot.
///
/// Matches `__relon_trace_resolve_call(ctx, external_addr_raw) ->
/// *const u8`. The cranelift emitter resolves the canonical symbol
/// via `JITBuilder::symbol`, but hosts (and v6-γ M5 partial-resume)
/// also need to indirect through the table to inspect / log call
/// resolution without re-deriving the symbol address. Keeping the
/// signature distinct from [`TraceHookFn`] lets the table populate
/// the slot without a lossy void-return shim.
pub type TraceResolveCallFn = unsafe extern "C" fn(*mut TraceContext, u64) -> *const u8;

/// `extern "C"` fn-pointer signature for the `inline_cache_lookup`
/// slot.
///
/// Matches `__relon_trace_inline_cache_lookup(ic_ptr, observed_type)
/// -> i32`. The IC fast path expects a raw `*mut u8` IC storage
/// pointer plus the observed-type discriminant; the return code
/// encodes `CacheResult::{Hit, Miss}` as `i32`. As with
/// [`TraceResolveCallFn`], the signature is kept distinct from the
/// uniform `TraceHookFn` shape so the table can carry the real fn
/// pointer without a shim that throws away the return value.
pub type TraceIcLookupFn = unsafe extern "C" fn(*mut u8, u8) -> i32;

/// Indirection table of host-side runtime helpers a trace may call
/// into.
///
/// Keeping the entries pointer-typed (not direct `fn`) lets the host
/// hot-swap implementations (e.g. profile-guided variants) without
/// recompiling installed traces.
///
/// ## Field semantics
///
/// - `on_trap` — invoked on any *unrecoverable* guard failure so the
///   host can log + abort. Always set; the runtime supplies a
///   default that simply records the failure into the `TraceContext`
///   then returns.
/// - `save_deopt` — invoked on every guard failure (just before the
///   trace tail-returns `GuardFailed`) to populate
///   `TraceContext::deopt_state`. Matches the
///   `__relon_trace_save_deopt` host symbol.
/// - `resolve_call` — invoked inside `TraceOp::Call` emission to
///   resolve a recorded callee id to its installed machine-code
///   pointer. Matches `__relon_trace_resolve_call`.
/// - `inline_cache_lookup` — invoked by the type-spec / inline-cache
///   fast path. Matches `__relon_trace_inline_cache_lookup`.
///
/// `#[repr(C)]` is load-bearing: the cranelift emitter loads
/// individual hook slots by raw byte offset from the
/// `TraceContext::host_hooks` field.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HostHookTable {
    /// Catch-all trap handler. The runtime always supplies a default;
    /// hosts may override to integrate with their own panic /
    /// telemetry pipeline.
    pub on_trap: Option<TraceHookFn>,
    /// `__relon_trace_save_deopt` host symbol address. v6-δ M1 R5
    /// widened to [`TraceSaveDeoptFn`] so the emitter's
    /// `call_indirect` carries the real `external_pc` arg through
    /// instead of dropping it via a shim.
    pub save_deopt: Option<TraceSaveDeoptFn>,
    /// `__relon_trace_resolve_call` host symbol address. v6-γ M5
    /// widened the signature from the uniform `TraceHookFn` to the
    /// helper's real shape — `(*mut TraceContext, u64) -> *const u8`
    /// — so populating the slot doesn't require a lossy void-return
    /// shim.
    pub resolve_call: Option<TraceResolveCallFn>,
    /// `__relon_trace_inline_cache_lookup` host symbol address.
    /// Widened in v6-γ M5 to the helper's real shape — `(*mut u8,
    /// u8) -> i32` — for the same reason as `resolve_call`.
    pub inline_cache_lookup: Option<TraceIcLookupFn>,
}

// SAFETY: `HostHookTable` only holds `Option<extern "C" fn>`
// pointers; sharing across threads is the host's responsibility once
// it installs concrete addresses.
unsafe impl Send for HostHookTable {}
unsafe impl Sync for HostHookTable {}

/// Slot count of the trace-local dict-lookup inline cache embedded
/// in [`TraceContext::dict_lookup_ic`]. Power-of-two so cranelift
/// can mask the slot index with a single `band` instead of a `urem`.
///
/// 16 is intentionally small: per-context residency dominates working
/// set on cold traces, and per-call entry to the helper amortises
/// the IC writeback only when the cache hit rate is meaningfully
/// above slot count / live key count. W5 (10 hot keys) sits well
/// inside the 16-slot budget; broader workloads degrade to the
/// helper call.
pub const DICT_LOOKUP_IC_SLOT_COUNT: usize = 16;

/// One slot of the inline cache the cranelift emitter probes before
/// calling `__relon_trace_dict_lookup_prechecked_v2`.
///
/// `#[repr(C)]` is load-bearing: the emitter accesses `dict_ptr`,
/// `key_ptr`, `value` via raw byte offsets resolved through
/// `mem::offset_of!` at lowering time. Field order is **fixed**.
///
/// ## Probe contract
///
/// - **Tag**: the pair `(dict_ptr, key_ptr)` of raw pointers. The
///   matching `DictShapeGuard` upstream already verified the dict's
///   shape header before the loop body, so a pointer-equal `dict_ptr`
///   identifies the same record envelope. The `key_ptr` is the
///   trace's incoming key SSA — for string-key dicts this is the
///   `StringRef` pointer, which is stable for as long as the trace's
///   arena holds the key alive.
/// - **Value**: the dict-lookup result the helper would have
///   returned. Cached on miss so subsequent hits skip the extern
///   call boundary entirely.
/// - **Empty slot**: all fields zero. The emitter initialises every
///   `TraceContext` IC array to zero and the host runtime never
///   stores a zero key/dict pointer (both are real heap addresses),
///   so a zero `dict_ptr` is a reliable "empty" sentinel.
///
/// ## ABA safety
///
/// In the production trace path, `dict_ptr` and `key_ptr` outlive
/// the trace (they live in the recorder's evaluator arenas), so
/// pointer identity is sufficient. Unit tests that synthesise dict
/// / key records on the stack inside a tight loop should call
/// [`TraceContext::reset_dict_lookup_ic`] between cases to avoid
/// false hits from address reuse.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DictIcSlot {
    /// Dict record pointer the slot caches a lookup for. Zero when
    /// the slot is empty.
    pub dict_ptr: u64,
    /// Key record pointer the slot caches a lookup for. Zero when
    /// the slot is empty.
    pub key_ptr: u64,
    /// Cached i64 value the helper returned for `(dict_ptr,
    /// key_ptr)`. Meaningful only when the slot is occupied.
    pub value: u64,
}

/// The runtime state every cranelift-emitted trace operates on.
///
/// **Layout invariant**: field order is load-bearing. See the module
/// docs for the canonical layout. The emitter encodes byte offsets as
/// cranelift constants at lowering time; reordering corrupts the
/// trace body / deopt path / result slot simultaneously.
#[repr(C)]
pub struct TraceContext {
    /// One slot per SSA var the trace produced. The emitter writes
    /// here via `ssa_slots[index] = value`, lowering through the
    /// `index * 8` byte offset of [`crate::ExternalSlot::byte_offset`].
    ///
    /// Placed first so the most frequent load (slot read in the
    /// trace body) is a zero-offset access off the context pointer.
    pub ssa_slots: Box<[u64]>,
    /// Result slot the trace writes its return value into on success.
    /// Always 64-bit wide; the cranelift backend widens narrower
    /// stores on emit.
    pub result_slot: u64,
    /// Populated by the guard-failure path on deopt; `None` while
    /// the trace is mid-execution.
    pub deopt_state: Option<DeoptStateSnapshot>,
    /// Host helper function pointers the cranelift trace may
    /// indirect-call into.
    pub host_hooks: HostHookTable,
    /// Pending recoverable writes; populated by store-fusion / DSE
    /// passes that emit `RecoverableWrite` ops at codegen time. The
    /// deopt path drains the entire vector into
    /// `deopt_state.recoverable_writes` and clears it.
    pub pending_recoverable_writes: Vec<RecoverableWriteRecord>,
    /// Per-context inline cache the cranelift emitter probes inline
    /// before falling through to the `__relon_trace_dict_lookup_*`
    /// helper. See [`DictIcSlot`] for the probe contract and slot
    /// layout.
    ///
    /// Sized to [`DICT_LOOKUP_IC_SLOT_COUNT`] so the emitter can
    /// mask the slot index with a single `band` instead of `urem`.
    pub dict_lookup_ic: [DictIcSlot; DICT_LOOKUP_IC_SLOT_COUNT],
}

impl TraceContext {
    /// Allocate a context with `slot_count` SSA slots zeroed.
    pub fn with_capacity(slot_count: usize) -> Self {
        Self {
            ssa_slots: vec![0u64; slot_count].into_boxed_slice(),
            result_slot: 0,
            deopt_state: None,
            host_hooks: HostHookTable::default(),
            pending_recoverable_writes: Vec::new(),
            dict_lookup_ic: [DictIcSlot::default(); DICT_LOOKUP_IC_SLOT_COUNT],
        }
    }

    /// Allocate a context with `slot_count` slots and the supplied
    /// [`HostHookTable`] pre-populated. Used by hosts that wire up
    /// `save_deopt` / `resolve_call` / `inline_cache_lookup` before
    /// invoking the trace.
    ///
    /// v6-γ M4: the cranelift emitter still resolves the hook
    /// symbols through `JITBuilder` (so the hooks live in the JIT
    /// module's symbol table, not in this `HostHookTable`). The
    /// table is kept populated in parallel so future emitter
    /// revisions that prefer indirect dispatch through the context
    /// can drop the symbol-resolution step without an ABI break.
    pub fn with_hooks(slot_count: usize, host_hooks: HostHookTable) -> Self {
        Self {
            ssa_slots: vec![0u64; slot_count].into_boxed_slice(),
            result_slot: 0,
            deopt_state: None,
            host_hooks,
            pending_recoverable_writes: Vec::new(),
            dict_lookup_ic: [DictIcSlot::default(); DICT_LOOKUP_IC_SLOT_COUNT],
        }
    }

    /// Clear all [`DICT_LOOKUP_IC_SLOT_COUNT`] dict-lookup IC slots
    /// back to the "empty" sentinel (`dict_ptr` / `key_ptr` / `value`
    /// all zero).
    ///
    /// Mostly used by unit tests that synthesise dict / key records
    /// on the stack inside a loop where address reuse would
    /// otherwise produce false IC hits. Production traces don't
    /// need to call this — the IC self-evicts naturally as new
    /// (dict, key) pairs hash into existing slots.
    pub fn reset_dict_lookup_ic(&mut self) {
        for slot in self.dict_lookup_ic.iter_mut() {
            *slot = DictIcSlot::default();
        }
    }

    /// Push a recoverable-write record onto the pending list. The
    /// emitter will eventually emit cranelift IR that calls a host
    /// helper to do this; the direct API is kept available for unit
    /// tests and host-side fallbacks.
    pub fn record_pending_write(&mut self, addr: u64, before_value: u64) {
        self.pending_recoverable_writes
            .push(RecoverableWriteRecord { addr, before_value });
    }
}

impl Default for TraceContext {
    fn default() -> Self {
        Self::with_capacity(0)
    }
}

impl std::fmt::Debug for TraceContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ic_occupied = self
            .dict_lookup_ic
            .iter()
            .filter(|s| s.dict_ptr != 0)
            .count();
        f.debug_struct("TraceContext")
            .field("ssa_slots_len", &self.ssa_slots.len())
            .field("result_slot", &self.result_slot)
            .field("deopt_state_some", &self.deopt_state.is_some())
            .field("host_hooks", &self.host_hooks)
            .field(
                "pending_recoverable_writes_len",
                &self.pending_recoverable_writes.len(),
            )
            .field("dict_lookup_ic_occupied", &ic_occupied)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_capacity_zeroes_slots() {
        let ctx = TraceContext::with_capacity(4);
        assert_eq!(ctx.ssa_slots.len(), 4);
        for v in ctx.ssa_slots.iter() {
            assert_eq!(*v, 0);
        }
        assert_eq!(ctx.result_slot, 0);
        assert!(ctx.deopt_state.is_none());
        assert!(ctx.pending_recoverable_writes.is_empty());
    }

    #[test]
    fn record_pending_write_appends_in_order() {
        let mut ctx = TraceContext::with_capacity(0);
        ctx.record_pending_write(0xaa, 1);
        ctx.record_pending_write(0xbb, 2);
        ctx.record_pending_write(0xcc, 3);
        let recs = &ctx.pending_recoverable_writes;
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].addr, 0xaa);
        assert_eq!(recs[1].addr, 0xbb);
        assert_eq!(recs[2].addr, 0xcc);
        assert_eq!(recs[0].before_value, 1);
        assert_eq!(recs[2].before_value, 3);
    }

    #[test]
    fn host_hook_table_default_is_all_none() {
        let t = HostHookTable::default();
        assert!(t.on_trap.is_none());
        assert!(t.save_deopt.is_none());
        assert!(t.resolve_call.is_none());
        assert!(t.inline_cache_lookup.is_none());
    }

    #[test]
    fn default_context_is_empty() {
        let ctx = TraceContext::default();
        assert!(ctx.ssa_slots.is_empty());
        assert_eq!(ctx.result_slot, 0);
        assert!(ctx.deopt_state.is_none());
    }

    #[test]
    fn dict_lookup_ic_starts_zeroed() {
        let ctx = TraceContext::with_capacity(0);
        assert_eq!(ctx.dict_lookup_ic.len(), DICT_LOOKUP_IC_SLOT_COUNT);
        for slot in ctx.dict_lookup_ic.iter() {
            assert_eq!(slot.dict_ptr, 0);
            assert_eq!(slot.key_ptr, 0);
            assert_eq!(slot.value, 0);
        }
    }

    #[test]
    fn reset_dict_lookup_ic_clears_all_slots() {
        let mut ctx = TraceContext::with_capacity(0);
        for (i, slot) in ctx.dict_lookup_ic.iter_mut().enumerate() {
            slot.dict_ptr = 0x1000 + i as u64;
            slot.key_ptr = 0x2000 + i as u64;
            slot.value = 0x3000 + i as u64;
        }
        ctx.reset_dict_lookup_ic();
        for slot in ctx.dict_lookup_ic.iter() {
            assert_eq!(slot.dict_ptr, 0);
            assert_eq!(slot.key_ptr, 0);
            assert_eq!(slot.value, 0);
        }
    }

    #[test]
    fn dict_ic_slot_layout_is_3x_u64() {
        // The emitter relies on these byte offsets when computing
        // `slot_addr + 0/8/16` for the inline probe.
        assert_eq!(std::mem::size_of::<DictIcSlot>(), 24);
        assert_eq!(std::mem::align_of::<DictIcSlot>(), 8);
        assert_eq!(std::mem::offset_of!(DictIcSlot, dict_ptr), 0);
        assert_eq!(std::mem::offset_of!(DictIcSlot, key_ptr), 8);
        assert_eq!(std::mem::offset_of!(DictIcSlot, value), 16);
    }
}
