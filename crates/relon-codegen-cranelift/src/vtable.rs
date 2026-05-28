//! v5-γ stage 2 capability-vtable indirection.
//!
//! Centralises the runtime contract between cranelift-emitted code
//! and the host's sandbox helpers. The codegen pass emits **indirect**
//! calls through a fixed-layout `__relon_capability_vtable` data
//! symbol; the host populates the table after `dlopen` (cached cold
//! start) or after `JITModule::finalize_definitions` (in-process JIT).
//!
//! ## Why indirection?
//!
//! Direct `extern "C"` calls work for `cranelift-jit` because the JIT
//! `symbol("relon_now", addr)` registers a per-process binding before
//! finalize. The same code emitted via `cranelift-object` becomes an
//! ELF import requiring runtime resolution. Resolving via the host
//! binary's dynamic-symbol table only works when the host links with
//! `-rdynamic`, which is fragile across `cargo test` / embedded host
//! integrations.
//!
//! Indirection makes the resolution explicit: the cached ET_DYN
//! references only one external symbol (the vtable data slot), and
//! the host fills the slots after `dlopen` returns. The same emission
//! works for the JIT path — the codegen declares the vtable as a
//! local-linkage data symbol the JIT populates after finalize.
//!
//! ## Layout
//!
//! `__relon_capability_vtable` is a `[*const u8; VtableSlot::COUNT]`
//! array. Each enum variant pins one slot index. Slot order is
//! **append-only**: bumping the count needs a `GENERATOR_VERSION`
//! bump in `object_cache_integration` so stale cache files
//! self-invalidate.
//!
//! ## Symbol
//!
//! - ELF: `__relon_capability_vtable` (exported as `Linkage::Export`
//!   on the object-emit path; `Linkage::Local` is fine for the JIT
//!   path because the JIT resolves data symbols by `DataId` not by
//!   name).
//! - Host: `populate_vtable(ptr)` writes the host-side function
//!   pointers into the slots. The function pointers are the same
//!   `SandboxState::*_helper` / `SandboxState::raise_trap` /
//!   `SandboxState::cap_lookup` Rust fns the JIT-symbol path
//!   used to register.

use crate::sandbox::SandboxState;

/// One slot per host helper the codegen indirects through. Adding a
/// new helper appends a variant (NEVER reorder existing variants);
/// the count constant below grows in step.
///
/// Layout (offset in bytes assumes 8-byte pointers):
///
/// | Slot | Offset | Symbol referenced                |
/// |------|--------|----------------------------------|
/// |  0   |   0    | `SandboxState::now_helper`       |
/// |  1   |   8    | `SandboxState::raise_trap`       |
/// |  2   |  16    | `SandboxState::cap_lookup`       |
///
/// The closure-table dispatch (`Op::CallClosure`) does **not** go
/// through this vtable: it loads the host fn pointer from the
/// per-evaluator `closure_table_base` field of `SandboxState`, which
/// is already an indirect address that survives dlopen unchanged.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtableSlot {
    /// `extern "C" fn(state: *const SandboxState) -> i64`
    RelonNow = 0,
    /// `extern "C" fn(state: *const SandboxState, code: i64)`
    RelonRaiseTrap = 1,
    /// `extern "C" fn(state: *const SandboxState, cap_bit: i32) -> *const u8`
    RelonCapLookup = 2,
    /// 2026-05-21: `extern "C" fn(s_ptr: *const u8, p_ptr: *const u8) -> i32`.
    ///
    /// Tier-2 LuaJIT-pattern-subset glob matcher. The two arguments
    /// are absolute pointers to wasm-style String records (`[len: u32
    /// LE][utf8 bytes]`); the helper reads the headers, decodes the
    /// UTF-8 payloads, runs the shared `relon_ir::glob::glob_match`
    /// algorithm, and returns `1` for match / `0` for no-match.
    ///
    /// Codegen intercepts `Op::Call { fn_index ==
    /// relon_ir::stdlib::GLOB_MATCH_INDEX }` in `emit_call_stdlib` and
    /// emits the indirect call here rather than inlining the bundled
    /// stdlib body (the body is a trap sentinel — see
    /// `relon_ir::stdlib::defs::glob_match_string`).
    RelonGlobMatch = 3,
}

impl VtableSlot {
    /// Number of slots reserved in the vtable. Bumping this requires
    /// a `GENERATOR_VERSION` bump in `object_cache_integration` so
    /// older cache files self-invalidate.
    pub const COUNT: u32 = 4;

    /// Byte offset of this slot inside the vtable. Each slot is one
    /// host pointer (8 bytes on x86_64-linux, which is v5-γ's only
    /// supported triple).
    pub fn offset_bytes(self) -> i32 {
        (self as u32) as i32 * 8
    }
}

/// Reserve `RESERVED_SLOTS` worth of bytes so future variants can be
/// appended without growing the cache-write path's data-section size
/// every time. `emit_entry_stub_object` allocates 32 slots today; we
/// keep that ceiling here to stay binary-compatible.
pub const RESERVED_SLOTS: u32 = 32;

/// Symbol name the codegen declares the vtable under. The host
/// `dlsym`s this name after `dlopen` to locate the table base.
pub const VTABLE_SYMBOL: &str = "__relon_capability_vtable";

/// Total byte size of the on-disk vtable data section. Uses the
/// 8-byte slot width because v5-γ ships Linux-x86_64 only.
pub const VTABLE_BYTES: usize = (RESERVED_SLOTS as usize) * 8;

/// Populate the vtable slots with host-side function pointers.
///
/// # Safety
///
/// - `vtable_ptr` must point at a writable region of at least
///   [`VTABLE_BYTES`] bytes.
/// - The pointer must remain valid (and the region writable) for the
///   lifetime of every cranelift module that references the vtable.
/// - The function pointers stored here must outlive any dlopen'd
///   ET_DYN that calls into them; since they're `&'static fn` items
///   inside the host binary, that holds as long as the host process
///   stays alive.
pub unsafe fn populate_vtable(vtable_ptr: *mut u8) {
    debug_assert!(
        !vtable_ptr.is_null(),
        "populate_vtable called with a null base"
    );
    let slots = vtable_ptr as *mut *const u8;
    // SAFETY: caller upholds the size invariant; we only write
    // `COUNT` slots (< RESERVED_SLOTS = 32).
    unsafe {
        *slots.add(VtableSlot::RelonNow as usize) = SandboxState::now_helper as *const u8;
        *slots.add(VtableSlot::RelonRaiseTrap as usize) = SandboxState::raise_trap as *const u8;
        *slots.add(VtableSlot::RelonCapLookup as usize) = SandboxState::cap_lookup as *const u8;
        *slots.add(VtableSlot::RelonGlobMatch as usize) =
            crate::glob_helper::relon_glob_match_helper as *const u8;
    }
    tracing::trace!(
        target: "relon::vtable",
        "populated vtable at {:p}: now={:p} raise_trap={:p} cap_lookup={:p} glob_match={:p}",
        vtable_ptr,
        SandboxState::now_helper as *const u8,
        SandboxState::raise_trap as *const u8,
        SandboxState::cap_lookup as *const u8,
        crate::glob_helper::relon_glob_match_helper as *const u8,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_offsets_are_distinct_and_packed() {
        assert_eq!(VtableSlot::RelonNow.offset_bytes(), 0);
        assert_eq!(VtableSlot::RelonRaiseTrap.offset_bytes(), 8);
        assert_eq!(VtableSlot::RelonCapLookup.offset_bytes(), 16);
        assert_eq!(VtableSlot::RelonGlobMatch.offset_bytes(), 24);
    }

    #[test]
    fn vtable_count_matches_variant_count() {
        // If a new variant is added without bumping `COUNT`, this
        // catches the inconsistency at test time.
        let variants = [
            VtableSlot::RelonNow,
            VtableSlot::RelonRaiseTrap,
            VtableSlot::RelonCapLookup,
            VtableSlot::RelonGlobMatch,
        ];
        assert_eq!(variants.len() as u32, VtableSlot::COUNT);
    }

    #[test]
    fn reserved_slots_dwarf_active_slot_count() {
        // Sanity-check at runtime so a future variant addition that
        // also bumps `COUNT` past `RESERVED_SLOTS` surfaces here. The
        // const arithmetic stops clippy's
        // `assertions_on_constants` lint from firing.
        let reserved = RESERVED_SLOTS;
        let count = VtableSlot::COUNT;
        assert!(
            reserved >= count,
            "RESERVED_SLOTS={reserved} must cover COUNT={count}",
        );
    }

    #[test]
    fn populate_vtable_writes_all_active_slot_pointers() {
        let mut buf = [0u8; VTABLE_BYTES];
        unsafe { populate_vtable(buf.as_mut_ptr()) };
        let slots = buf.as_ptr() as *const *const u8;
        unsafe {
            assert!(!(*slots.add(0)).is_null(), "RelonNow slot");
            assert!(!(*slots.add(1)).is_null(), "RelonRaiseTrap slot");
            assert!(!(*slots.add(2)).is_null(), "RelonCapLookup slot");
            assert!(!(*slots.add(3)).is_null(), "RelonGlobMatch slot");
        }
    }
}
