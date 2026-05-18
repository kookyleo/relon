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
//!
//! Putting `ssa_slots` first lets the emitter address it through a
//! zero-offset load of the context pointer — the most frequent
//! operation in the trace body. `result_slot` follows so its offset
//! (16 = sizeof(Box fat ptr)) is also constant-known at emit time.

use crate::deopt::{DeoptStateSnapshot, RecoverableWriteRecord};

/// `extern "C"` function pointer type used by every entry of
/// [`HostHookTable`].
///
/// The two-argument shape — `(*mut TraceContext, u32)` — is shared
/// across every hook so the emitter can dispatch through a single
/// indirect call instruction without per-hook signature shuffling.
/// Hooks needing extra args bundle them through the
/// `TraceContext::pending_recoverable_writes` / `ssa_slots` side
/// channels (the recorded address + before-value already covers the
/// recoverable-write hook; the IC lookup hook reads its key out of
/// a designated slot index passed as the `u32` payload).
pub type TraceHookFn = unsafe extern "C" fn(*mut TraceContext, u32);

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
    /// `__relon_trace_save_deopt` host symbol address.
    pub save_deopt: Option<TraceHookFn>,
    /// `__relon_trace_resolve_call` host symbol address.
    pub resolve_call: Option<TraceHookFn>,
    /// `__relon_trace_inline_cache_lookup` host symbol address.
    pub inline_cache_lookup: Option<TraceHookFn>,
}

// SAFETY: `HostHookTable` only holds `Option<extern "C" fn>`
// pointers; sharing across threads is the host's responsibility once
// it installs concrete addresses.
unsafe impl Send for HostHookTable {}
unsafe impl Sync for HostHookTable {}

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
        f.debug_struct("TraceContext")
            .field("ssa_slots_len", &self.ssa_slots.len())
            .field("result_slot", &self.result_slot)
            .field("deopt_state_some", &self.deopt_state.is_some())
            .field("host_hooks", &self.host_hooks)
            .field(
                "pending_recoverable_writes_len",
                &self.pending_recoverable_writes.len(),
            )
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
}
