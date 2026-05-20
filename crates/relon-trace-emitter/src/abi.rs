//! Trace-entry ABI surface (cranelift-specific helpers).
//!
//! Shared ABI types are sourced from [`relon_trace_abi`] — `TraceContext`,
//! `DeoptStateSnapshot`, `RecoverableWriteRecord`, `TRACE_ENTRY_SIG`,
//! `AbiSignature`, `AbiType`, `TraceEntryStatus`, `HostHookTable`,
//! `ExternalPc` / `ExternalSlot` / `ExternalAddr`, `ObservedType`,
//! `EffectClass`. This module re-exports them so existing
//! `crate::abi::TraceContext` imports keep working and adds the
//! cranelift-specific helpers the ABI crate can't provide without
//! pulling cranelift into its dep graph.
//!
//! ## What stays cranelift-specific here
//!
//! - [`abi_type_to_cranelift`] — `AbiType -> cranelift_codegen::ir::Type`
//!   resolver. Lives here, not in `relon-trace-abi`, so the ABI crate
//!   keeps zero cranelift dep.
//! - [`AbiSignatureExt::to_cranelift`] — extension trait that turns an
//!   `AbiSignature` into a concrete `ir::Signature`. Same rationale.
//! - [`HostHookId`] — symbolic id for the four host helper functions
//!   the emitter imports into the cranelift module. Stays emitter-side
//!   because it's a codegen-time concern, not an ABI invariant.
//!
//! Every consumer outside this crate should continue to spell types
//! as `crate::abi::TraceContext`, `crate::abi::TraceEntryStatus`, etc.
//! The shared layout lives in `relon-trace-abi`; this is the cranelift
//! lens onto it.

use cranelift_codegen::ir;

pub use relon_trace_abi::{
    AbiSignature, AbiType, DeoptStateSnapshot, EffectClass, ExternalAddr, ExternalPc, ExternalSlot,
    HostHookTable, ObservedType, RecoverableWriteRecord, TraceContext, TraceEntryStatus,
    TRACE_ENTRY_SIG,
};

/// Backwards-compatible alias for the old emitter-local `CraneliftType`
/// enum. Reviewers: prefer [`AbiType`] in new code; this alias is kept
/// so existing call sites compile without touch-up. The cranelift
/// dispatch goes through [`abi_type_to_cranelift`] regardless.
pub type CraneliftType = AbiType;

/// Backwards-compatible newtype around [`ExternalPc`]. Same FFI
/// representation (`u64`); kept so the emitter's public re-export
/// stays available for downstream consumers that have not migrated to
/// the [`ExternalPc`] direct import yet.
pub type ExternalPcRepr = ExternalPc;

/// Backwards-compatible newtype around [`ExternalSlot`]. Same FFI
/// representation (`u32`).
pub type ExternalSlotRepr = ExternalSlot;

/// Backwards-compatible newtype around [`ExternalAddr`]. Same FFI
/// representation (`u64`).
pub type ExternalAddrRepr = ExternalAddr;

/// Resolve an [`AbiType`] to its cranelift `ir::Type` counterpart given
/// the host's pointer width.
///
/// Lives in the emitter crate (not `relon-trace-abi`) so the ABI crate
/// stays cranelift-agnostic — see the module docs.
pub fn abi_type_to_cranelift(ty: AbiType, pointer_ty: ir::Type) -> ir::Type {
    match ty {
        AbiType::I32 => ir::types::I32,
        AbiType::I64 => ir::types::I64,
        AbiType::Ptr => pointer_ty,
    }
}

/// Extension trait that lowers an [`AbiSignature`] to a concrete
/// cranelift [`ir::Signature`]. SystemV is hard-coded because that's
/// the only calling convention `relon-codegen-native` ever emits;
/// threading `CallConv` through [`AbiSignature`] would re-introduce a
/// cranelift dep into the ABI crate.
pub trait AbiSignatureExt {
    /// Lower to a concrete cranelift signature using the host target's
    /// pointer width and the supplied calling convention.
    fn to_cranelift(
        &self,
        pointer_ty: ir::Type,
        call_conv: cranelift_codegen::isa::CallConv,
    ) -> ir::Signature;
}

impl AbiSignatureExt for AbiSignature {
    fn to_cranelift(
        &self,
        pointer_ty: ir::Type,
        call_conv: cranelift_codegen::isa::CallConv,
    ) -> ir::Signature {
        let mut sig = ir::Signature::new(call_conv);
        for p in self.params {
            sig.params
                .push(ir::AbiParam::new(abi_type_to_cranelift(*p, pointer_ty)));
        }
        for r in self.returns {
            sig.returns
                .push(ir::AbiParam::new(abi_type_to_cranelift(*r, pointer_ty)));
        }
        sig
    }
}

/// Byte offset of [`TraceContext::result_slot`] inside the shared
/// `#[repr(C)] TraceContext` layout. The emitter uses this when
/// lowering `TraceOp::Return` so the resulting cranelift IR stores
/// through the correct field, **not** offset 0.
///
/// The trace-abi crate keeps `ssa_slots: Box<[u64]>` as field 0 (so
/// the hot per-op load is zero-offset off the context pointer); the
/// fat pointer occupies 16 bytes on every supported target, putting
/// `result_slot` at byte 16. Computed via `mem::offset_of!` so any
/// future field-order change in `relon-trace-abi` re-emits here
/// automatically.
pub const fn result_slot_offset() -> i32 {
    std::mem::offset_of!(TraceContext, result_slot) as i32
}

/// Byte offset of [`TraceContext::host_hooks`]. v6-δ M1 R5 uses this
/// to load the `HostHookTable` so the emitter can dispatch `save_deopt`
/// / `resolve_call` / `inline_cache_lookup` via `call_indirect` instead
/// of a direct extern call. Indirecting through the table lets hosts
/// hot-swap helpers (profile-guided / instrumented variants) without
/// recompiling installed traces.
pub const fn host_hooks_offset() -> i32 {
    std::mem::offset_of!(TraceContext, host_hooks) as i32
}

/// Byte offset of the supplied [`HostHookId`] slot **inside** the
/// embedded `HostHookTable`. Combine with [`host_hooks_offset`] for
/// the full byte offset off a `TraceContext` pointer:
///
/// ```text
/// hook_ptr = load.i64(ctx + host_hooks_offset() + host_hook_slot_offset(id))
/// ```
pub fn host_hook_slot_offset(hook: HostHookId) -> i32 {
    use relon_trace_abi::HostHookTable;
    let off = match hook {
        HostHookId::SaveDeopt => std::mem::offset_of!(HostHookTable, save_deopt),
        HostHookId::ResolveCall => std::mem::offset_of!(HostHookTable, resolve_call),
        HostHookId::InlineCacheLookup => std::mem::offset_of!(HostHookTable, inline_cache_lookup),
        // F-D7 str hooks are NOT routed through the `HostHookTable`
        // — they're imported directly via [`HostHookFuncIds`] so the
        // emitter can `call` them without a per-op pointer load.
        // Callers that ask for an offset have a bug.
        HostHookId::StrConcat
        | HostHookId::StrConcatAlloc
        | HostHookId::StrContains
        | HostHookId::StrFind
        | HostHookId::StrSubstring => {
            panic!("host_hook_slot_offset called for F-D7 str hook {:?}; str hooks do not sit in HostHookTable", hook)
        }
        // F-D8 hooks are dispatched via direct symbol resolution, not
        // through `HostHookTable`. The two-level table indirection
        // exists so hosts can hot-swap `save_deopt` / `resolve_call`
        // / `inline_cache_lookup` per-context; the dict/list helpers
        // are read-only fast-paths whose hot-swap semantics aren't
        // required for F-D8 v1. Callers that ask for a slot offset
        // on these hooks have a bug — return a sentinel that will
        // surface as `EmitError::HostHookNotDeclared` if reached.
        HostHookId::ListGet | HostHookId::DictLookup | HostHookId::DictLookupPrechecked => {
            debug_assert!(
                false,
                "host_hook_slot_offset called for ListGet/DictLookup/DictLookupPrechecked; \
                 F-D8 hooks do not live in HostHookTable"
            );
            return -1;
        }
    };
    off as i32
}

/// Stable id of a host hook the emitter may reference when importing
/// runtime helper functions into the cranelift module.
///
/// Emitter-side concern — the discriminant order is not part of the
/// ABI between the emitted IR and the runtime helpers; it's only used
/// inside the emitter as a `UserExternalName::index` so downstream
/// linker / debugger tooling can match imports back to symbolic names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostHookId {
    /// `__relon_trace_save_deopt`.
    SaveDeopt,
    /// `__relon_trace_resolve_call`.
    ResolveCall,
    /// `__relon_trace_inline_cache_lookup`.
    InlineCacheLookup,
    /// F-D7: `__relon_str_concat(*const u8, usize, *const u8, usize, *mut StrRet)`.
    /// String shim helpers are imported through the same hook table
    /// machinery so the emitter can issue a direct `call` to them
    /// without an `__relon_trace_resolve_call` round-trip on every
    /// op. Resolution is performed once at install time via
    /// [`HostHookFuncIds`].
    StrConcat,
    /// F-D7-I: `__relon_str_concat_alloc(lhs: *const StringRef,
    /// total_len: usize) -> *mut StringRef`. Allocator-only helper for
    /// the inline `StrConcat` lowering — the cranelift IR uses this to
    /// reserve a fresh `StringRef` whose payload buffer already holds
    /// the `lhs` bytes, then inlines unrolled stores for the const
    /// rhs tail. See
    /// [`relon_trace_jit::runtime::__relon_str_concat_alloc`] for the
    /// payload-and-buffer ownership contract.
    StrConcatAlloc,
    /// F-D7: `__relon_str_contains(*const u8, usize, *const u8, usize) -> i32`.
    StrContains,
    /// F-D7: `__relon_str_find(*const u8, usize, *const u8, usize) -> i64`.
    StrFind,
    /// F-D7: `__relon_str_substring(*const u8, usize, i64, i64, *mut StrRet)`.
    StrSubstring,
    /// F-D8: `__relon_trace_list_get(list_ptr: *const u8, idx: i64,
    /// ctx: *mut TraceContext) -> i64`. Bounds-checked indexed access
    /// into a `[len: u32 LE][pad: u32][i64 elements...]` record. On
    /// out-of-range access the helper writes a sentinel into
    /// `ctx.result_slot` and returns 0; the cranelift-side guard
    /// branches into the deopt block.
    ListGet,
    /// F-D8: `__relon_trace_dict_lookup(dict_ptr: *const u8, key_ptr:
    /// *const u8, shape_hash: u64, ctx: *mut TraceContext) -> i64`.
    /// IC-guarded dict access: on shape match the helper returns the
    /// cached i64 value; on mismatch it returns a sentinel that the
    /// surrounding cranelift IR turns into a deopt branch so the
    /// recorder gets a chance to re-specialise under the new shape.
    DictLookup,
    /// F-D8-E.2: `__relon_trace_dict_lookup_prechecked(dict_ptr:
    /// *const u8, key_ptr: *const u8, ctx: *mut TraceContext) -> i64`.
    /// Same semantics as [`Self::DictLookup`] except the helper
    /// skips the shape compare on the IC fast path. The cranelift
    /// emitter lowers `TraceOp::DictLookupPrechecked` into a call
    /// to this helper; the matching `TraceOp::DictShapeGuard` ahead
    /// of the lookup (typically hoisted out of the enclosing loop
    /// by LICM) keeps the safety contract intact.
    DictLookupPrechecked,
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
            HostHookId::StrConcat => "__relon_str_concat",
            HostHookId::StrConcatAlloc => "__relon_str_concat_alloc",
            HostHookId::StrContains => "__relon_str_contains",
            HostHookId::StrFind => "__relon_str_find",
            HostHookId::StrSubstring => "__relon_str_substring",
            HostHookId::ListGet => "__relon_trace_list_get",
            HostHookId::DictLookup => "__relon_trace_dict_lookup",
            HostHookId::DictLookupPrechecked => "__relon_trace_dict_lookup_prechecked",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::isa::CallConv;

    #[test]
    fn trace_entry_sig_shape() {
        assert_eq!(TRACE_ENTRY_SIG.params.len(), 2);
        assert_eq!(TRACE_ENTRY_SIG.returns.len(), 1);
        assert_eq!(TRACE_ENTRY_SIG.params[0], AbiType::Ptr);
        assert_eq!(TRACE_ENTRY_SIG.params[1], AbiType::Ptr);
        assert_eq!(TRACE_ENTRY_SIG.returns[0], AbiType::I32);
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
        assert_eq!(abi_type_to_cranelift(AbiType::I32, ptr64), ir::types::I32);
        assert_eq!(abi_type_to_cranelift(AbiType::I64, ptr64), ir::types::I64);
        assert_eq!(abi_type_to_cranelift(AbiType::Ptr, ptr64), ir::types::I64);
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
        assert_eq!(ExternalSlot(0).byte_offset(), 0);
        assert_eq!(ExternalSlot(1).byte_offset(), 8);
        assert_eq!(ExternalSlot(10).byte_offset(), 80);
    }
}
