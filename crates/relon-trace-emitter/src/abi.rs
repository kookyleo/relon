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
