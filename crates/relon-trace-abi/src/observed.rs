//! Observed-type buckets the trace recorder learns by spying on the
//! cranelift-generic backend's tagged values.
//!
//! Shared ABI type. trace-jit / trace-emitter / codegen-native all
//! import this enum rather than redeclaring it. Phase v6-γ M1 starts
//! requiring every shared type live **only** in this crate; the ABI
//! smoke tests will reject any fork-definition.
//!
//! ## Bucket choice
//!
//! Five buckets, picked so the inline-cache fast path can branch on a
//! single 3-bit tag. The bucket assignment matches the trace JIT's
//! historical (`trace_ir.rs`) enum byte-for-byte: the discriminant
//! integers are the on-wire representation of [`ObservedType`] used
//! by the inline-cache header that lives in the cranelift constant
//! pool.
//!
//! Reorder = ABI break. Add new buckets at the end of the list so
//! existing discriminants stay stable.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Concrete observed type for an SSA value.
///
/// The recorder pins these by sampling the generic backend's tagged
/// values; the optimiser uses them to specialise arithmetic and the
/// inline-cache helper uses them to key its lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[repr(u8)]
pub enum ObservedType {
    /// 32-bit signed integer.
    I32 = 0,
    /// 64-bit signed integer.
    I64 = 1,
    /// IEEE-754 double precision.
    F64 = 2,
    /// 1-bit boolean. Stored widened to a slot-width integer.
    Bool = 3,
    /// Pointer-like tag. Opaque to the optimizer; treated as
    /// `!= I32`.
    Ptr = 4,
}

impl ObservedType {
    /// Wire-format discriminant; matches the value the inline-cache
    /// header stores in the cranelift constant pool. Stable across
    /// versions.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parse from the wire-format discriminant. Returns `None` for
    /// unknown values so callers can decide whether to treat an
    /// unknown bucket as a cache miss vs. abort.
    pub fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(ObservedType::I32),
            1 => Some(ObservedType::I64),
            2 => Some(ObservedType::F64),
            3 => Some(ObservedType::Bool),
            4 => Some(ObservedType::Ptr),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminants_are_stable() {
        // Locked to match `relon_trace_jit::trace_ir::ObservedType` so
        // phase M1 can swap the import path without re-recording any
        // golden trace dumps.
        assert_eq!(ObservedType::I32 as u8, 0);
        assert_eq!(ObservedType::I64 as u8, 1);
        assert_eq!(ObservedType::F64 as u8, 2);
        assert_eq!(ObservedType::Bool as u8, 3);
        assert_eq!(ObservedType::Ptr as u8, 4);
    }

    #[test]
    fn roundtrip_all_buckets() {
        for ot in [
            ObservedType::I32,
            ObservedType::I64,
            ObservedType::F64,
            ObservedType::Bool,
            ObservedType::Ptr,
        ] {
            assert_eq!(ObservedType::from_u8(ot.as_u8()), Some(ot));
        }
    }

    #[test]
    fn unknown_discriminant_returns_none() {
        // Anything past the highest defined variant must be reported
        // as `None` so the IC helper can treat it as a miss rather
        // than aliasing onto a real bucket.
        assert!(ObservedType::from_u8(5).is_none());
        assert!(ObservedType::from_u8(0xff).is_none());
    }
}
