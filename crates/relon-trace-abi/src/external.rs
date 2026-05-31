//! External-world handles the cranelift-emitted trace shuffles across
//! the FFI boundary.
//!
//! Shared ABI types. trace-jit / trace-emitter / codegen-cranelift all
//! import these newtypes rather than redeclaring them. Phase v6-γ M1
//! starts requiring every shared type live **only** in this crate;
//! any fork-definition will be rejected by ABI tests.
//!
//! ## Representation pin
//!
//! - [`ExternalPc`] is `#[repr(transparent)] u64`, the address of a
//!   `*const u8` instruction pointer into the cranelift-generic
//!   backend's machine code, **cast to `u64` for FFI portability** so
//!   the trace context layout has no platform-pointer-width gap.
//! - [`ExternalSlot`] is `#[repr(transparent)] u32`, an index into
//!   `TraceContext::ssa_slots`. 32-bit so the deopt-write helper can
//!   take it in a register without overflow handling.
//! - [`ExternalAddr`] is `#[repr(transparent)] u64`, an opaque memory
//!   address (cast `*mut u8`). High bits **reserved** for a future
//!   width tag (phase M2 decision; today MUST be zero).
//!
//! ### Phase decision: width-tag bits on [`ExternalAddr`]
//!
//! v5-β-2 leaves the high bits unused; v6-γ phase M2 may pack a
//! 3-bit width tag into the top so the recoverable-write replay
//! helper can dispatch by width without an extra table lookup.
//! Until then, helpers below assert the high bits stay zero so
//! recorded traces are forward-compatible.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Opaque program-counter handle into the cranelift-generic backend
/// where a trace must "deopt out to" on guard failure.
///
/// Representation: raw `*const u8` instruction pointer **cast to
/// `u64`** so the field has a fixed FFI width regardless of host
/// pointer size. The cast is lossless on every supported target
/// (x86_64 / aarch64 — both 64-bit pointers).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ExternalPc(pub u64);

impl ExternalPc {
    /// Construct from a raw pointer. Lossless on supported targets.
    pub fn from_ptr(p: *const u8) -> Self {
        Self(p as u64)
    }

    /// Recover the pointer view. Safe to call; dereferencing the
    /// returned pointer requires the host's usual guarantees about
    /// the lifetime of the generic-backend code page.
    pub fn as_ptr(self) -> *const u8 {
        self.0 as *const u8
    }
}

/// Index into [`crate::TraceContext::ssa_slots`].
///
/// Representation: 32-bit. The full slot table is bounded far below
/// 2^29 in practice (LuaJIT-style traces cap at a few thousand SSA
/// vars), so 32 bits leaves plenty of headroom while keeping the
/// deopt-write helper's argument tight against typical SystemV
/// register schedules.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ExternalSlot(pub u32);

impl ExternalSlot {
    /// One slot is 8 bytes wide; every consumer assumes the slot
    /// table is `Box<[u64]>`.
    pub const SLOT_WIDTH_BYTES: i32 = 8;

    /// Byte offset of `ssa_slots[index]` from the start of the
    /// `ssa_slots` heap payload. The cranelift emitter loads the
    /// payload base first, then indexes off of it using this value.
    pub fn byte_offset(self) -> i32 {
        // 32-bit slot id * 8 bytes per slot; trace lengths are bounded
        // far below 2^28 so the multiplication can never overflow i32
        // in practice. `wrapping_mul` keeps the low 32 bits, matching the
        // truncating `as i32` of an i64 product bit-for-bit, and
        // self-documents that wrapping is safe here.
        (self.0 as i32).wrapping_mul(Self::SLOT_WIDTH_BYTES)
    }

    /// Recover the raw u32 index, e.g. for serialisation.
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

/// Opaque memory address whose mutation must be undone on deopt.
///
/// Typically a scratch arena cursor address or a list-append length
/// slot. The recorder treats it as a plain identifier; the host
/// knows how to interpret it.
///
/// Representation: `*mut u8` cast to `u64`. The cast is lossless on
/// supported targets.
///
/// **Reserved bits**: today the entire `u64` is the address. The
/// high 3 bits are reserved for a future width tag (phase M2). Until
/// the tag lands, [`ExternalAddr::from_ptr`] asserts the high bits
/// are zero in debug builds so we catch any pointer-tagging tricks
/// the host may try to sneak in.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ExternalAddr(pub u64);

impl ExternalAddr {
    /// Number of high bits reserved for a future width tag. Today
    /// **always zero**; phase M2 may flip some on. The const lives
    /// here so the recoverable-write replay helper can mask them
    /// off in one branch-free op once they go live.
    pub const RESERVED_HIGH_BITS: u32 = 3;

    /// Mask of the bits currently used to encode the address. The
    /// remaining high bits are reserved (see type-level docs).
    pub const ADDR_MASK: u64 =
        !(((1u64 << Self::RESERVED_HIGH_BITS) - 1) << (64 - Self::RESERVED_HIGH_BITS));

    /// Construct from a raw pointer. Lossless on supported targets.
    pub fn from_ptr(p: *mut u8) -> Self {
        let raw = p as u64;
        // Most user-space pointers on x86_64 / aarch64 have zero in
        // the top 16 bits today, so this assertion is effectively a
        // no-op for ordinary heap addresses. It only fires when the
        // host hands us a pointer with the reserved bits set —
        // which would silently collide with the phase M2 width tag.
        debug_assert!(
            raw & !Self::ADDR_MASK == 0,
            "ExternalAddr: high {} bits must be zero (reserved for future width tag)",
            Self::RESERVED_HIGH_BITS,
        );
        Self(raw)
    }

    /// Recover the pointer view.
    pub fn as_ptr(self) -> *mut u8 {
        (self.0 & Self::ADDR_MASK) as *mut u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_pc_roundtrip_through_ptr() {
        // Use a heap-rooted u8 so the pointer is unambiguously valid;
        // we never actually deref it.
        let backing = [0u8; 1];
        let p = backing.as_ptr();
        let pc = ExternalPc::from_ptr(p);
        assert_eq!(pc.as_ptr(), p);
    }

    #[test]
    fn external_pc_roundtrip_through_u64() {
        // Synthetic addresses (used by golden tests) must round-trip
        // through the integer view too.
        let pc = ExternalPc(0xdead_beef_cafe_babe);
        assert_eq!(pc.as_ptr() as u64, 0xdead_beef_cafe_babe);
    }

    #[test]
    fn external_slot_byte_offset_is_8x_index() {
        assert_eq!(ExternalSlot(0).byte_offset(), 0);
        assert_eq!(ExternalSlot(1).byte_offset(), 8);
        assert_eq!(ExternalSlot(10).byte_offset(), 80);
        assert_eq!(ExternalSlot(1024).byte_offset(), 8192);
    }

    #[test]
    fn external_addr_roundtrip() {
        let mut backing = [0u8; 1];
        let p = backing.as_mut_ptr();
        let addr = ExternalAddr::from_ptr(p);
        assert_eq!(addr.as_ptr(), p);
    }

    #[test]
    fn external_slot_as_u32_preserves_value() {
        assert_eq!(ExternalSlot(0).as_u32(), 0);
        assert_eq!(ExternalSlot(u32::MAX).as_u32(), u32::MAX);
    }
}
