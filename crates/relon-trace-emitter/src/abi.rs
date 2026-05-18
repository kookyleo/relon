//! Trace-entry ABI surface.
//!
//! Every cranelift function emitted by [`crate::TraceEmitter`] respects
//! the same fixed signature so the host can install trace pointers
//! through a uniform dispatch slot. The shape mirrors a "managed entry"
//! that takes a runtime context + input arg buffer and returns a
//! status word the dispatcher inspects.
//!
//! ```text
//! fn trace_entry(trace_ctx: *mut TraceContext, input_args: *const Value)
//!     -> i32 {
//!     // 0 = success, output written to trace_ctx.result_slot
//!     // 1 = guard failed, deopt info written to trace_ctx.deopt_state
//!     // 2 = trace aborted (recoverable; the dispatcher should fall back
//!     //     to the generic backend without recording the path)
//! }
//! ```
//!
//! ## ExternalPc / ExternalSlot / ExternalAddr binding
//!
//! The trace-jit scaffolding keeps those three handles intentionally
//! opaque (they're plain `u64`-tagged newtypes today). The emitter has
//! to commit to a concrete in-memory representation so it can emit
//! cranelift IR that reads / writes them. We bind:
//!
//! - [`ExternalPc`](relon_trace_jit::ExternalPc) ⇒ raw `*const u8`
//!   instruction pointer into the cranelift-generic backend. Treated
//!   as an opaque pointer; the emitter only ever stores it into
//!   `TraceContext::deopt_state` via the runtime helper.
//! - [`ExternalSlot`](relon_trace_jit::ExternalSlot) ⇒ `u32` index into
//!   `TraceContext::ssa_slots`. Compact so the deopt-write helper can
//!   take it as a plain register argument.
//! - [`ExternalAddr`](relon_trace_jit::ExternalAddr) ⇒ raw `*mut u8`
//!   memory location. Used by `recoverable_writes` replay.
//!
//! ### TODO (v6-gamma phase decision)
//!
//! Whether to keep the `u32` slot index *as is* or pack a 32-bit
//! "shift + lane" pair (à la LuaJIT's `IRRef`) is undecided. The same
//! goes for tagging the high bits of `ExternalAddr` with a type
//! discriminator (`TYPE_TAG`) so the replay helper can dispatch by
//! width without an extra table lookup. The const-fn newtypes the
//! scaffolding picks for representation are deliberately conservative
//! — flip them to packed values once we have benchmark numbers on
//! cache pressure inside the deopt path.

use cranelift_codegen::ir;

/// Cranelift IR type tag the emitter understands. Wrapping the small
/// subset of `ir::Type` we care about lets the ABI module compile
/// without pulling cranelift internals into doctests / public docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CraneliftType {
    /// 32-bit signed integer.
    I32,
    /// 64-bit signed integer.
    I64,
    /// Target pointer (e.g. 64-bit on x86_64 / aarch64).
    Ptr,
}

impl CraneliftType {
    /// Resolve to a concrete cranelift type given the host pointer width.
    pub fn resolve(self, pointer_ty: ir::Type) -> ir::Type {
        match self {
            CraneliftType::I32 => ir::types::I32,
            CraneliftType::I64 => ir::types::I64,
            CraneliftType::Ptr => pointer_ty,
        }
    }
}

/// Abstract cranelift signature description. We carry the trace-entry
/// shape as a `'static` constant so the host can compare against it
/// during dispatch slot installation without re-constructing a
/// `Signature` from scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiSignature {
    /// Argument types, left-to-right.
    pub params: &'static [CraneliftType],
    /// Return types. Trace entries always return exactly one i32.
    pub returns: &'static [CraneliftType],
}

impl AbiSignature {
    /// Lower to a concrete cranelift [`ir::Signature`] using the host
    /// target's pointer width and the SystemV calling convention (the
    /// only one v5-beta-1 codegen-native ever emits).
    pub fn to_cranelift(
        &self,
        pointer_ty: ir::Type,
        call_conv: cranelift_codegen::isa::CallConv,
    ) -> ir::Signature {
        let mut sig = ir::Signature::new(call_conv);
        for p in self.params {
            sig.params.push(ir::AbiParam::new(p.resolve(pointer_ty)));
        }
        for r in self.returns {
            sig.returns.push(ir::AbiParam::new(r.resolve(pointer_ty)));
        }
        sig
    }
}

/// Fixed signature every trace entry obeys.
///
/// `(trace_ctx: *mut TraceContext, input_args: *const Value) -> i32`.
pub const TRACE_ENTRY_SIG: AbiSignature = AbiSignature {
    params: &[CraneliftType::Ptr, CraneliftType::Ptr],
    returns: &[CraneliftType::I32],
};

/// Status word a trace entry returns. Aligned with the doc comment on
/// the entry signature above; the dispatcher branches on these values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum TraceEntryStatus {
    /// Trace ran to completion; result slot populated.
    Success = 0,
    /// A guard failed; `TraceContext::deopt_state` is populated with
    /// the snapshot needed to resume generic execution.
    GuardFailed = 1,
    /// Trace aborted before completing — the host should fall through
    /// to the generic backend without recording the path. Reserved for
    /// future use by length / recursion limits.
    Aborted = 2,
}

impl TraceEntryStatus {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Runtime concrete representation of the trace context the entry
/// receives a pointer to. Field order is **load-bearing**: the emitter
/// reads/writes by byte offset.
#[derive(Debug, Default)]
#[repr(C)]
pub struct TraceContext {
    /// Result slot the entry writes its return value into when it
    /// completes successfully. Always 64-bit wide (the cranelift
    /// backend widens narrower values on store).
    pub result_slot: u64,
    /// One slot per SSA var the trace produced. Indexed by
    /// `ExternalSlot` (= `u32`).
    pub ssa_slots: Box<[u64]>,
    /// Populated by the deopt path on guard failure.
    pub deopt_state: Option<DeoptStateSnapshot>,
    /// Indirection table of host helper functions emitted calls into.
    pub host_hooks: HostHookTable,
}

impl TraceContext {
    /// Allocate a context with `slot_count` SSA slots zeroed.
    pub fn with_capacity(slot_count: usize) -> Self {
        Self {
            result_slot: 0,
            ssa_slots: vec![0u64; slot_count].into_boxed_slice(),
            deopt_state: None,
            host_hooks: HostHookTable::default(),
        }
    }

    /// Byte offset of `result_slot` inside `TraceContext`. The emitter
    /// uses this with `mem_flags` loads + stores to read/write the
    /// slot through the `*mut TraceContext` it receives as arg 0.
    pub const fn result_slot_offset() -> i32 {
        // SAFETY: repr(C); result_slot is the first field, offset 0.
        0
    }
}

/// Snapshot the deopt path leaves behind for the dispatcher / generic
/// backend to consume. Mirrors `relon_trace_jit::DeoptState` but lives
/// in `#[repr(C)]` memory so the cranelift emitter can manipulate it
/// directly through the trace context pointer.
///
/// TODO (v6-gamma phase decision): this is currently a thin wrapper;
/// the v6-gamma integration phase will likely fuse it with the
/// `relon_trace_jit::DeoptState` representation so guards can hand off
/// the captured state without an intermediate copy.
#[derive(Debug, Default, Clone)]
pub struct DeoptStateSnapshot {
    /// `trace_pc` of the guard that fired. Lets the host helper find
    /// the matching [`relon_trace_jit::GuardSite`] in the optimised
    /// trace's side tables.
    pub guard_trace_pc: u32,
    /// External PC the dispatcher must jump to. Bound to `*const u8`;
    /// stored as `u64` for FFI portability.
    pub external_pc: u64,
}

/// Table of host-side runtime helpers a trace may call into. The
/// emitter resolves these symbolically (by [`HostHookId`]) when
/// importing function references; the integration phase populates the
/// table with real fn pointers.
///
/// Keeping the table pointer-typed (not direct `fn`) lets the runtime
/// hot-swap helpers (e.g. profile-guided variants) without
/// recompiling installed traces.
#[derive(Debug, Default, Clone)]
pub struct HostHookTable {
    /// Address of `__relon_trace_save_deopt(ctx, guard_trace_pc,
    /// external_pc)`. Called by the deopt block right before the
    /// trace returns `GuardFailed`.
    pub save_deopt: Option<*const u8>,
    /// Address of `__relon_trace_resolve_call(func_id) -> *const u8`.
    /// Called inside `TraceOp::Call` emission to fetch the callee's
    /// machine-code pointer at runtime.
    pub resolve_call: Option<*const u8>,
    /// Address of `__relon_trace_inline_cache_lookup(cache_id, key)
    /// -> u64`. Reserved for the type-spec / IC fast-path.
    pub inline_cache_lookup: Option<*const u8>,
}

// SAFETY: `HostHookTable` only holds opaque `*const u8` function
// pointers; sharing across threads is the host's responsibility.
unsafe impl Send for HostHookTable {}
unsafe impl Sync for HostHookTable {}

/// Stable id of a host hook the emitter may reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostHookId {
    /// `__relon_trace_save_deopt`.
    SaveDeopt,
    /// `__relon_trace_resolve_call`.
    ResolveCall,
    /// `__relon_trace_inline_cache_lookup`.
    InlineCacheLookup,
}

impl HostHookId {
    /// Symbolic name the host uses when registering the hook in its
    /// cranelift module. Kept stable so external tooling (linkers /
    /// profilers) can reference the trace ABI by name.
    pub fn symbol(self) -> &'static str {
        match self {
            HostHookId::SaveDeopt => "__relon_trace_save_deopt",
            HostHookId::ResolveCall => "__relon_trace_resolve_call",
            HostHookId::InlineCacheLookup => "__relon_trace_inline_cache_lookup",
        }
    }
}

/// `ExternalPc` representation bound by the emitter. Distinct newtype
/// so callers can't accidentally pass a wrong-width integer into the
/// deopt machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalPcRepr(pub *const u8);

// SAFETY: `ExternalPcRepr` is an opaque address into pre-compiled
// generic code; it is `Send` because the host pins the code pages.
unsafe impl Send for ExternalPcRepr {}
unsafe impl Sync for ExternalPcRepr {}

/// `ExternalSlot` representation bound by the emitter. `u32` keeps
/// the slot table dense and the deopt-write helper's register
/// pressure low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExternalSlotRepr(pub u32);

impl ExternalSlotRepr {
    /// Byte offset of `ssa_slots[index]` inside `TraceContext`
    /// computed at runtime via the layout of `Box<[u64]>`'s heap-
    /// allocated payload. The emitter loads the `ssa_slots` pointer
    /// out of the context first and then indexes off of it.
    pub const SLOT_WIDTH_BYTES: i32 = 8;

    pub fn byte_offset(self) -> i32 {
        // 32-bit slot id * 8 bytes per slot; trace lengths are bounded
        // far below 2^29 so the multiplication can never overflow i32.
        self.0 as i32 * Self::SLOT_WIDTH_BYTES
    }
}

/// `ExternalAddr` representation bound by the emitter. Raw `*mut u8`;
/// reserved high bits are kept zero pending a v6-gamma decision on
/// whether to pack a type tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalAddrRepr(pub *mut u8);

// SAFETY: see `ExternalPcRepr`.
unsafe impl Send for ExternalAddrRepr {}
unsafe impl Sync for ExternalAddrRepr {}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::isa::CallConv;

    #[test]
    fn trace_entry_sig_shape() {
        assert_eq!(TRACE_ENTRY_SIG.params.len(), 2);
        assert_eq!(TRACE_ENTRY_SIG.returns.len(), 1);
        assert_eq!(TRACE_ENTRY_SIG.params[0], CraneliftType::Ptr);
        assert_eq!(TRACE_ENTRY_SIG.params[1], CraneliftType::Ptr);
        assert_eq!(TRACE_ENTRY_SIG.returns[0], CraneliftType::I32);
    }

    #[test]
    fn entry_status_discriminants() {
        assert_eq!(TraceEntryStatus::Success.as_i32(), 0);
        assert_eq!(TraceEntryStatus::GuardFailed.as_i32(), 1);
        assert_eq!(TraceEntryStatus::Aborted.as_i32(), 2);
    }

    #[test]
    fn cranelift_type_resolution() {
        let ptr64 = ir::types::I64;
        assert_eq!(CraneliftType::I32.resolve(ptr64), ir::types::I32);
        assert_eq!(CraneliftType::I64.resolve(ptr64), ir::types::I64);
        assert_eq!(CraneliftType::Ptr.resolve(ptr64), ir::types::I64);
    }

    #[test]
    fn abi_signature_lowers_to_cranelift() {
        let pointer_ty = ir::types::I64;
        let sig = TRACE_ENTRY_SIG.to_cranelift(pointer_ty, CallConv::SystemV);
        assert_eq!(sig.params.len(), 2);
        assert_eq!(sig.returns.len(), 1);
        assert_eq!(sig.params[0].value_type, ir::types::I64);
        assert_eq!(sig.returns[0].value_type, ir::types::I32);
    }

    #[test]
    fn host_hook_id_symbols_are_stable() {
        assert_eq!(HostHookId::SaveDeopt.symbol(), "__relon_trace_save_deopt");
        assert_eq!(
            HostHookId::ResolveCall.symbol(),
            "__relon_trace_resolve_call"
        );
        assert_eq!(
            HostHookId::InlineCacheLookup.symbol(),
            "__relon_trace_inline_cache_lookup"
        );
    }

    #[test]
    fn trace_context_zero_init_round_trip() {
        let ctx = TraceContext::with_capacity(4);
        assert_eq!(ctx.result_slot, 0);
        assert_eq!(ctx.ssa_slots.len(), 4);
        assert!(ctx.deopt_state.is_none());
    }

    #[test]
    fn external_slot_byte_offset_is_8x_index() {
        assert_eq!(ExternalSlotRepr(0).byte_offset(), 0);
        assert_eq!(ExternalSlotRepr(1).byte_offset(), 8);
        assert_eq!(ExternalSlotRepr(10).byte_offset(), 80);
    }
}
