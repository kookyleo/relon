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
    /// `extern "C" fn(state: *const SandboxState, import_idx: u32,
    /// args_ptr: *const i64, arg_count: u32) -> i64`.
    ///
    /// Dynamic host-fn dispatch for source-lowered `Op::CallNative`
    /// whose `cap_bit` is `NO_CAPABILITY_BIT`. Resolves the
    /// `Arc<dyn RelonFunction>` registered at `import_idx`, packs the
    /// scalar args, and invokes it. See [`SandboxState::call_native`].
    RelonCallNative = 4,
    /// `extern "C" fn(state: *const SandboxState) -> i64`.
    ///
    /// Built-in `clock()` primitive (`Op::ReadClock`). Reads the host
    /// wall clock (`SystemTime::now()`) and returns the count of
    /// nanoseconds since the Unix epoch. Distinct from
    /// [`Self::RelonNow`], which is the monotonic deadline probe.
    RelonClockWall = 5,
    /// `extern "C" fn(state: *const SandboxState) -> i64`.
    ///
    /// Built-in `random()` primitive (`Op::ReadRandom`). Returns 8
    /// fresh random bytes packed into an `i64` (host OS entropy).
    RelonRandom = 6,
    /// `extern "C" fn(state: *const SandboxState, path_off: i32) -> i32`.
    ///
    /// Built-in `read_file(path)` primitive (`Op::ReadFile`), P-fs
    /// Stage 1. `path_off` is the arena-relative offset of the path's
    /// wasm-style String record (`[len: u32 LE][utf8 bytes]`). The
    /// helper reads the path out of the arena, resolves it against the
    /// shared filesystem sandbox root (`relon_util`), reads the file,
    /// bump-allocates a fresh String record at `tail_cursor`, and
    /// returns its arena-relative offset (or a negative sentinel on a
    /// sandbox-escape / I/O failure, which the codegen turns into a
    /// trap).
    RelonReadFile = 7,
    /// `extern "C" fn(state: *const SandboxState, path_off: i32) -> i32`.
    ///
    /// Built-in `read_dir(path)` primitive (`Op::ReadDir`), P-fs
    /// Stage 2. `path_off` is the arena-relative offset of the path's
    /// wasm-style String record (`[len: u32 LE][utf8 bytes]`). The
    /// helper reads the path out of the arena, resolves it against the
    /// shared filesystem sandbox root (`relon_util`), lists the
    /// directory's entry names, SORTS them (byte-lexicographic, for
    /// cross-backend determinism), bump-allocates a `List<String>`
    /// pointer-array record at `tail_cursor` (element String records
    /// then a `[len][off_0]...` header, the `Op::ConstListString`
    /// layout), and returns the header's arena-relative offset (or a
    /// negative sentinel on a sandbox-escape / I/O failure, which the
    /// codegen turns into a trap).
    RelonReadDir = 8,
    /// `extern "C" fn(state: *const SandboxState, path_off: i32) -> i32`.
    ///
    /// Built-in `stat(path)` primitive (`Op::Stat`), P-fs Stage 3.
    /// `path_off` is the arena-relative offset of the path's wasm-style
    /// String record (`[len: u32 LE][utf8 bytes]`). The helper reads the
    /// path out of the arena, resolves it against the shared filesystem
    /// sandbox root (`relon_util`), reads the metadata
    /// (`std::fs::metadata`), bump-allocates a `{is_dir: Bool, size: Int}`
    /// dict record at `tail_cursor` (the `Op::ConstDict` layout), and
    /// returns the record's arena-relative offset (or a negative sentinel
    /// on a sandbox-escape / I/O failure, which the codegen turns into a
    /// trap).
    RelonStat = 9,
}

impl VtableSlot {
    /// Number of slots reserved in the vtable. Bumping this requires
    /// a `GENERATOR_VERSION` bump in `object_cache_integration` so
    /// older cache files self-invalidate.
    pub const COUNT: u32 = 10;

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
        *slots.add(VtableSlot::RelonCallNative as usize) = SandboxState::call_native as *const u8;
        *slots.add(VtableSlot::RelonClockWall as usize) =
            SandboxState::clock_wall_helper as *const u8;
        *slots.add(VtableSlot::RelonRandom as usize) = SandboxState::random_helper as *const u8;
        *slots.add(VtableSlot::RelonReadFile as usize) =
            crate::read_file_helper::relon_read_file_helper as *const u8;
        *slots.add(VtableSlot::RelonReadDir as usize) =
            crate::read_dir_helper::relon_read_dir_helper as *const u8;
        *slots.add(VtableSlot::RelonStat as usize) =
            crate::stat_helper::relon_stat_helper as *const u8;
    }
    tracing::trace!(
        target: "relon::vtable",
        "populated vtable at {:p}: now={:p} raise_trap={:p} cap_lookup={:p} glob_match={:p} call_native={:p} clock_wall={:p} random={:p} read_file={:p} read_dir={:p} stat={:p}",
        vtable_ptr,
        SandboxState::now_helper as *const u8,
        SandboxState::raise_trap as *const u8,
        SandboxState::cap_lookup as *const u8,
        crate::glob_helper::relon_glob_match_helper as *const u8,
        SandboxState::call_native as *const u8,
        SandboxState::clock_wall_helper as *const u8,
        SandboxState::random_helper as *const u8,
        crate::read_file_helper::relon_read_file_helper as *const u8,
        crate::read_dir_helper::relon_read_dir_helper as *const u8,
        crate::stat_helper::relon_stat_helper as *const u8,
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
        assert_eq!(VtableSlot::RelonCallNative.offset_bytes(), 32);
        assert_eq!(VtableSlot::RelonClockWall.offset_bytes(), 40);
        assert_eq!(VtableSlot::RelonRandom.offset_bytes(), 48);
        assert_eq!(VtableSlot::RelonReadFile.offset_bytes(), 56);
        assert_eq!(VtableSlot::RelonReadDir.offset_bytes(), 64);
        assert_eq!(VtableSlot::RelonStat.offset_bytes(), 72);
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
            VtableSlot::RelonCallNative,
            VtableSlot::RelonClockWall,
            VtableSlot::RelonRandom,
            VtableSlot::RelonReadFile,
            VtableSlot::RelonReadDir,
            VtableSlot::RelonStat,
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
            assert!(!(*slots.add(4)).is_null(), "RelonCallNative slot");
            assert!(!(*slots.add(5)).is_null(), "RelonClockWall slot");
            assert!(!(*slots.add(6)).is_null(), "RelonRandom slot");
            assert!(!(*slots.add(7)).is_null(), "RelonReadFile slot");
            assert!(!(*slots.add(8)).is_null(), "RelonReadDir slot");
            assert!(!(*slots.add(9)).is_null(), "RelonStat slot");
        }
    }
}
