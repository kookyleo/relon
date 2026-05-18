//! Cranelift-agnostic description of the trace-entry function ABI.
//!
//! Shared ABI types. trace-jit / trace-emitter / codegen-native all
//! import these definitions rather than redeclaring them. Phase v6-γ
//! M1 starts requiring every shared type live **only** in this crate;
//! any fork-definition will be rejected by ABI tests.
//!
//! ## What lives here vs. in `relon-trace-emitter::abi`
//!
//! `relon-trace-emitter::abi` historically defined a `CraneliftType`
//! enum wrapping `cranelift_codegen::ir::Type`. That coupled the trace
//! ABI surface to cranelift internals — pulling cranelift into every
//! crate that wanted to type-check the trace ABI.
//!
//! This crate keeps the **cranelift-agnostic** half (the
//! [`AbiType`] enum + [`AbiSignature`] + [`TRACE_ENTRY_SIG`]) and the
//! emitter will, once it migrates to this crate in phase M1, provide
//! a small helper:
//!
//! ```ignore
//! fn map(abi: AbiType, pointer_ty: ir::Type) -> ir::Type {
//!     match abi {
//!         AbiType::I32 => ir::types::I32,
//!         AbiType::I64 => ir::types::I64,
//!         AbiType::Ptr => pointer_ty,
//!     }
//! }
//! ```
//!
//! No cranelift dep here.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Width tag the emitter understands. A deliberately tiny set: we
/// only ever cross the FFI boundary with `i32`-status codes and
/// pointer-typed `TraceContext` / argument-buffer handles.
///
/// `I64` is included for forward compatibility (e.g. once trace
/// returns get widened or extra metadata is threaded through the
/// signature) but is **not** part of the canonical
/// [`TRACE_ENTRY_SIG`] today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[repr(u8)]
pub enum AbiType {
    /// Target pointer (e.g. 64-bit on x86_64 / aarch64). The
    /// cranelift emitter resolves this to the host's actual pointer
    /// type at lowering time.
    Ptr = 0,
    /// 32-bit signed integer (status word).
    I32 = 1,
    /// 64-bit signed integer.
    I64 = 2,
}

/// Cranelift-agnostic signature description.
///
/// We carry the trace-entry shape as a `'static` constant so the host
/// can compare against it during dispatch slot installation without
/// reconstructing a `Signature` from scratch.
///
/// Reviewers: **DO NOT** add a calling-convention field here. The
/// trace ABI pins SystemV (the only one v5-β-1 codegen-native ever
/// emits) and the emitter applies it at lowering time. Threading
/// `CallConv` through this struct would re-introduce a cranelift dep
/// into the ABI crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiSignature {
    /// Argument types, left-to-right.
    pub params: &'static [AbiType],
    /// Return types. Trace entries always return exactly one `I32`.
    pub returns: &'static [AbiType],
}

/// Canonical signature every trace entry obeys.
///
/// ```text
/// fn trace_entry(
///     trace_ctx:   *mut crate::TraceContext,
///     input_args:  *const u64,        // host-defined value buffer
/// ) -> i32 {
///     // 0 = success, output written to trace_ctx.result_slot
///     // 1 = guard failed, deopt info written to trace_ctx.deopt_state
///     // 2 = trace aborted (recoverable; the dispatcher falls back to
///     //     the generic backend without recording the path)
/// }
/// ```
///
/// See [`TraceEntryStatus`] for the return-code semantics.
pub const TRACE_ENTRY_SIG: AbiSignature = AbiSignature {
    params: &[AbiType::Ptr, AbiType::Ptr],
    returns: &[AbiType::I32],
};

/// Status word a trace entry returns.
///
/// Discriminant ordering is **load-bearing**: the cranelift emitter
/// hard-codes these integer values into the trace return path and
/// the host dispatcher branches on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[repr(i32)]
pub enum TraceEntryStatus {
    /// Trace ran to completion; result slot populated.
    Success = 0,
    /// A guard failed; `TraceContext::deopt_state` is populated with
    /// the snapshot needed to resume generic execution.
    GuardFailed = 1,
    /// Trace aborted before completing — the host should fall through
    /// to the generic backend without recording the path. Reserved
    /// for future use by length / recursion limits.
    Aborted = 2,
}

impl TraceEntryStatus {
    /// Project to the raw `i32` the trace returns at the FFI boundary.
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Parse from the raw FFI value the trace returned. Returns
    /// `None` for unknown values so the dispatcher can treat an
    /// out-of-band status as a forced abort rather than aliasing
    /// onto a real bucket.
    pub fn from_i32(raw: i32) -> Option<Self> {
        match raw {
            0 => Some(TraceEntryStatus::Success),
            1 => Some(TraceEntryStatus::GuardFailed),
            2 => Some(TraceEntryStatus::Aborted),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_entry_sig_shape() {
        assert_eq!(TRACE_ENTRY_SIG.params.len(), 2);
        assert_eq!(TRACE_ENTRY_SIG.returns.len(), 1);
        assert_eq!(TRACE_ENTRY_SIG.params[0], AbiType::Ptr);
        assert_eq!(TRACE_ENTRY_SIG.params[1], AbiType::Ptr);
        assert_eq!(TRACE_ENTRY_SIG.returns[0], AbiType::I32);
    }

    #[test]
    fn entry_status_discriminants_stable() {
        // Hard-coded into emitter & dispatcher; reviewers must update
        // both sides simultaneously if these ever shift.
        assert_eq!(TraceEntryStatus::Success.as_i32(), 0);
        assert_eq!(TraceEntryStatus::GuardFailed.as_i32(), 1);
        assert_eq!(TraceEntryStatus::Aborted.as_i32(), 2);
    }

    #[test]
    fn entry_status_roundtrip() {
        for s in [
            TraceEntryStatus::Success,
            TraceEntryStatus::GuardFailed,
            TraceEntryStatus::Aborted,
        ] {
            assert_eq!(TraceEntryStatus::from_i32(s.as_i32()), Some(s));
        }
        assert!(TraceEntryStatus::from_i32(3).is_none());
        assert!(TraceEntryStatus::from_i32(-1).is_none());
    }
}
