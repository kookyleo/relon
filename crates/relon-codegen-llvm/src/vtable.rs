//! Host-helper indirection table for the LLVM-native AOT backend.
//! **Phase C.**
//!
//! The cranelift backend (`relon-codegen-cranelift::vtable`) is the gold
//! standard: it centralises the runtime contract between emitted code
//! and the host's sandbox helpers behind a fixed-layout
//! `__relon_capability_vtable` data symbol the codegen indirects through
//! and the host populates after `dlopen` / JIT-finalize.
//!
//! ## Why the LLVM table resolves by *symbol*, not by a data slot
//!
//! cranelift's object-emit path turns a direct `extern "C"` call into an
//! ELF import that needs runtime resolution against the host's
//! dynamic-symbol table — fragile unless the host links `-rdynamic`.
//! Cranelift dodges that with a data-vtable the host fills after dlopen.
//!
//! The LLVM backend already resolves host helpers a different way: the
//! emitted module declares each helper as an `extern` function under a
//! stable symbol name, and the evaluator maps that name onto the host
//! fn's address with `ExecutionEngine::add_global_mapping` before
//! resolving the entry pointer (see `state::RELON_LLVM_CALL_NATIVE_SYMBOL`
//! / `str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL`). For the
//! linked-after **native** object the same symbols become ordinary
//! undefined externs the linker resolves against the host binary (the
//! `relon-rs-shims` staticlib provides them), so no data-vtable
//! indirection is needed.
//!
//! This module therefore ports cranelift's `VtableSlot` enum + populate
//! surface to the LLVM model as a **symbol registry**: one [`VtableSlot`]
//! per host helper, each carrying the stable symbol name the emitted
//! module declares and the host address `add_global_mapping` binds. The
//! slot *order* mirrors cranelift's so a side-by-side audit lines up;
//! the carrier differs (symbol name vs data-section offset).

use crate::state::{relon_llvm_call_native_addr, RELON_LLVM_CALL_NATIVE_SYMBOL};
use crate::str_helpers::{
    relon_llvm_f64_to_str_addr, relon_llvm_str_contains_arena_addr, RELON_LLVM_F64_TO_STR_SYMBOL,
    RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL,
};

/// One slot per host helper the LLVM codegen indirects through, in the
/// same order cranelift pins them in its data-vtable. Adding a new
/// helper appends a variant (NEVER reorder existing variants).
///
/// | Slot | cranelift analogue            | LLVM symbol                          |
/// |------|-------------------------------|--------------------------------------|
/// |  0   | `RelonGlobMatch` (slot 3)     | `relon_llvm_str_contains_arena`      |
/// |  1   | `RelonCallNative` (slot 4)    | `relon_llvm_call_native`             |
/// |  2   | `RelonF64ToStr` (slot 5)      | `relon_llvm_f64_to_str`              |
///
/// cranelift's slots 0..=2 (`RelonNow` / `RelonRaiseTrap` /
/// `RelonCapLookup`) have no LLVM counterpart: the LLVM gate is an
/// inline `caps`-bitmask test baked into the object by `Op::CheckCap`
/// (no `cap_lookup` helper), trap codes are written directly to
/// `ArenaState::trap_code` by the helper / trap arm (no `raise_trap`
/// helper), and the deadline clock (`now`) is reserved for the LLVM
/// deadline work. Only the two helpers the LLVM emitter actually
/// declares as externs are represented here.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtableSlot {
    /// `extern "C" fn(s_ptr: *const u8, n_ptr: *const u8) -> i32`.
    /// Tier-2 substring matcher; the LLVM mirror of cranelift's
    /// `RelonGlobMatch`. Declared lazily on the first
    /// `Op::Call { contains }` site.
    RelonStrContains = 0,
    /// `extern "C" fn(state: *const ArenaState, import_idx: u32,
    /// args_ptr: *const i64, arg_count: u32) -> i64`. Dynamic host-fn
    /// dispatch; the LLVM mirror of cranelift's `RelonCallNative`. See
    /// [`crate::state::relon_llvm_call_native`].
    RelonCallNative = 1,
    /// `extern "C" fn(bits: i64, dest: *mut u8) -> i32`. Wave B float
    /// Display renderer; the LLVM mirror of cranelift's
    /// `RelonF64ToStr`. Declared lazily on the first `Op::FloatToStr`
    /// site. See [`crate::str_helpers::relon_llvm_f64_to_str`].
    RelonF64ToStr = 2,
}

impl VtableSlot {
    /// Number of slots the LLVM emitter can declare. Mirrors cranelift's
    /// `VtableSlot::COUNT`; bumping it needs a matching variant + a
    /// `populate_global_mappings` arm.
    pub const COUNT: u32 = 3;

    /// All slots, in declaration order. Used by [`populate_global_mappings`]
    /// and the parity tests.
    pub const ALL: [VtableSlot; Self::COUNT as usize] = [
        VtableSlot::RelonStrContains,
        VtableSlot::RelonCallNative,
        VtableSlot::RelonF64ToStr,
    ];

    /// Stable symbol name the emitted LLVM module declares this helper
    /// under. The host binds it via `add_global_mapping` (JIT) or the
    /// linker resolves it against `relon-rs-shims` (native object).
    pub fn symbol(self) -> &'static str {
        match self {
            VtableSlot::RelonStrContains => RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL,
            VtableSlot::RelonCallNative => RELON_LLVM_CALL_NATIVE_SYMBOL,
            VtableSlot::RelonF64ToStr => RELON_LLVM_F64_TO_STR_SYMBOL,
        }
    }

    /// Host-side address of the helper backing this slot, as a `usize`
    /// suitable for `ExecutionEngine::add_global_mapping`. The cranelift
    /// analogue is the `*const u8` fn pointer `populate_vtable` writes
    /// into the data slot.
    pub fn host_addr(self) -> usize {
        match self {
            VtableSlot::RelonStrContains => relon_llvm_str_contains_arena_addr(),
            VtableSlot::RelonCallNative => relon_llvm_call_native_addr(),
            VtableSlot::RelonF64ToStr => relon_llvm_f64_to_str_addr(),
        }
    }
}

/// Resolve every slot to its `(symbol, host_addr)` binding. The
/// evaluator iterates this to register `add_global_mapping`s for the
/// helpers the emitted module actually references (it only declares a
/// helper's extern on first use, so the caller filters by
/// `module.get_function(symbol).is_some()` before binding). The LLVM
/// analogue of cranelift's `populate_vtable`, which writes every active
/// slot's fn pointer into the data section unconditionally.
///
/// Returned addresses are non-null (`&'static` host fn items) and stay
/// valid for the host process lifetime.
pub fn populate_global_mappings() -> [(&'static str, usize); VtableSlot::COUNT as usize] {
    [
        (
            VtableSlot::RelonStrContains.symbol(),
            VtableSlot::RelonStrContains.host_addr(),
        ),
        (
            VtableSlot::RelonCallNative.symbol(),
            VtableSlot::RelonCallNative.host_addr(),
        ),
        (
            VtableSlot::RelonF64ToStr.symbol(),
            VtableSlot::RelonF64ToStr.host_addr(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_count_matches_variant_list() {
        assert_eq!(VtableSlot::ALL.len() as u32, VtableSlot::COUNT);
    }

    #[test]
    fn slot_indices_are_distinct_and_packed() {
        assert_eq!(VtableSlot::RelonStrContains as u32, 0);
        assert_eq!(VtableSlot::RelonCallNative as u32, 1);
        assert_eq!(VtableSlot::RelonF64ToStr as u32, 2);
    }

    #[test]
    fn symbols_are_stable_and_match_state_str_helpers() {
        assert_eq!(
            VtableSlot::RelonStrContains.symbol(),
            RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL
        );
        assert_eq!(
            VtableSlot::RelonCallNative.symbol(),
            RELON_LLVM_CALL_NATIVE_SYMBOL
        );
        assert_eq!(
            VtableSlot::RelonF64ToStr.symbol(),
            RELON_LLVM_F64_TO_STR_SYMBOL
        );
    }

    #[test]
    fn host_addrs_are_non_null_and_stable() {
        for slot in VtableSlot::ALL {
            let a = slot.host_addr();
            assert_ne!(a, 0, "{slot:?} host addr must be non-null");
            // Stable across calls (mirrors the `_addr()` helper contract).
            assert_eq!(a, slot.host_addr(), "{slot:?} host addr must be stable");
        }
    }

    #[test]
    fn populate_global_mappings_covers_every_active_slot() {
        let mappings = populate_global_mappings();
        assert_eq!(mappings.len() as u32, VtableSlot::COUNT);
        for (sym, addr) in mappings {
            assert!(!sym.is_empty(), "symbol name must be non-empty");
            assert_ne!(addr, 0, "host addr must be non-null for {sym}");
        }
        // Every slot maps to a distinct symbol.
        assert_ne!(mappings[0].0, mappings[1].0);
        assert_ne!(mappings[0].0, mappings[2].0);
        assert_ne!(mappings[1].0, mappings[2].0);
    }
}
