//! Host helper backing the cranelift `Op::Stat` lowering — the built-in
//! `stat(path) -> Dict` primitive (P-fs Stage 3).
//!
//! The cranelift codegen lowers `Op::Stat` to an indirect call through
//! the [`crate::vtable::VtableSlot::RelonStat`] slot. The capability gate
//! (`reads_fs`) already fired in the preceding `Op::CheckCap`.
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn relon_stat_helper(
//!     state:    *const SandboxState,
//!     path_off: i32,   // arena-relative offset of the path String record
//! ) -> i32             // arena-relative offset of the dict record, or a
//!                      // negative sentinel on failure
//! ```
//!
//! The input path is a wasm-style String record `[len: u32 LE][utf8
//! bytes]` at `arena_base + path_off`. The helper:
//!
//!   1. reads the path bytes out of the arena (bounds-checked);
//!   2. resolves the path against the shared filesystem sandbox root
//!      (`relon_util::resolve_fs_sandbox_path`) — refusing escapes;
//!   3. reads the metadata (`std::fs::metadata`);
//!   4. bump-allocates a `{is_dir: Bool, size: Int}` dict record in the
//!      scratch region (the `Op::ConstDict` layout — see
//!      [`crate::codegen::const_pool`]: a `[entry_count][pad][shape_hash]`
//!      header, then sorted `[key_off][key_len][value]` entries, then the
//!      concatenated key payload), and returns the record's arena-relative
//!      offset.
//!
//! On any failure (malformed path record, sandbox escape, I/O error,
//! arena overflow) the helper records a [`TrapKind`] into the sandbox
//! trap slot and returns a negative sentinel; the codegen turns the
//! negative result into the standard trap epilogue.

use crate::sandbox::{SandboxState, TrapKind};

/// Negative sentinel the codegen checks for: any negative return means
/// "the stat failed; the trap slot holds the reason".
const STAT_FAILURE: i32 = -1;

/// Read the `(len, payload_addr)` of a wasm-style String record at
/// `arena_base + off`. Returns `None` when the header / payload walks
/// past `arena_base + arena_len`.
///
/// # Safety
///
/// `arena_base` + `arena_len` must describe a live, host-owned arena
/// segment (populated by [`SandboxState::install_arena`]).
unsafe fn read_record(arena_base: usize, arena_len: u32, off: i32) -> Option<(u32, *const u8)> {
    if off < 0 {
        return None;
    }
    let off_u = off as u32;
    let header_end = off_u.checked_add(4)?;
    if header_end > arena_len {
        return None;
    }
    let header_addr = arena_base.checked_add(off_u as usize)?;
    // SAFETY: bounds checked against `arena_len` above.
    let len_bytes = unsafe { std::ptr::read_unaligned(header_addr as *const u32) };
    let payload_end = header_end.checked_add(len_bytes)?;
    if payload_end > arena_len {
        return None;
    }
    let payload_addr = arena_base.checked_add((off_u + 4) as usize)?;
    Some((len_bytes, payload_addr as *const u8))
}

/// Cranelift-callable helper for `Op::Stat`.
///
/// # Safety
///
/// * `state` must point at a live, properly aligned [`SandboxState`]
///   with an arena installed via [`SandboxState::install_arena`].
/// * `path_off` must be an arena-relative offset the codegen produced
///   for an `IrType::String` value.
pub(crate) unsafe extern "C" fn relon_stat_helper(
    state: *const SandboxState,
    path_off: i32,
) -> i32 {
    if state.is_null() {
        return STAT_FAILURE;
    }
    // SAFETY: the caller upholds the SandboxState invariants.
    let state_ref = unsafe { &*state };
    let arena_base = state_ref.arena_base();
    let arena_len = state_ref.arena_len();
    if arena_base == 0 {
        unsafe { record_trap(state_ref, TrapKind::Unreachable) };
        return STAT_FAILURE;
    }

    // 1. Read the path bytes out of the arena.
    let (path_len, path_ptr) = match unsafe { read_record(arena_base, arena_len, path_off) } {
        Some(v) => v,
        None => {
            unsafe { record_trap(state_ref, TrapKind::Unreachable) };
            return STAT_FAILURE;
        }
    };
    // SAFETY: `read_record` validated the payload range.
    let path_bytes = unsafe { std::slice::from_raw_parts(path_ptr, path_len as usize) };
    let path = match std::str::from_utf8(path_bytes) {
        Ok(v) => v,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::Unreachable) };
            return STAT_FAILURE;
        }
    };

    // 2. Resolve against the sandbox root (refuses escapes).
    let resolved = match relon_util::resolve_fs_sandbox_path(path) {
        Ok(p) => p,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::CapabilityDenied) };
            return STAT_FAILURE;
        }
    };

    // 3. Read the metadata.
    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::CapabilityDenied) };
            return STAT_FAILURE;
        }
    };
    let is_dir = meta.is_dir();
    let size = meta.len() as i64;

    // 4. Bump-allocate the dict record into the scratch region.
    let scratch_base = unsafe { state_ref.scratch_base() };
    let pre_cursor = unsafe { state_ref.scratch_cursor() };
    match unsafe {
        materialize_stat_dict(
            arena_base,
            arena_len,
            scratch_base,
            pre_cursor,
            is_dir,
            size,
        )
    } {
        Some((record_off, new_cursor)) => {
            unsafe { state_ref.set_scratch_cursor(new_cursor) };
            match i32::try_from(record_off) {
                Ok(v) => v,
                Err(_) => {
                    unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
                    STAT_FAILURE
                }
            }
        }
        None => {
            unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
            STAT_FAILURE
        }
    }
}

/// Write the `{is_dir: Bool, size: Int}` dict record into the scratch
/// region at `scratch_base + pre_cursor` (8-aligned, so the i64 values +
/// u64 shape_hash land on natural boundaries — matching
/// `const_pool::visit_const_dict`). Returns `(record_off,
/// new_scratch_cursor)` on success (arena-relative offset,
/// scratch-relative cursor), or `None` on arena / u32 overflow.
///
/// Layout (record-relative offsets, mirroring `Op::ConstDict`):
///
/// ```text
/// [entry_count: u32 @0][pad: u32 @4][shape_hash: u64 @8]   ; 16-byte header
/// entry_count × [key_off: u32][key_len: u32][value: i64]   ; 16 bytes each,
///                                                          ;   sorted by key
/// concatenated UTF-8 key bytes                             ; key_off is
///                                                          ;   record-relative
/// ```
///
/// The two keys (`is_dir`, `size`) are sorted byte-lexicographically
/// (`is_dir` < `size`). `is_dir`'s `Bool` is stored as an i64 0/1.
///
/// # Safety
///
/// `[arena_base, arena_base + arena_len)` must be a live, host-owned,
/// writable arena segment for the duration of the call.
unsafe fn materialize_stat_dict(
    arena_base: usize,
    arena_len: u32,
    scratch_base: u32,
    pre_cursor: u32,
    is_dir: bool,
    size: i64,
) -> Option<(u32, u32)> {
    // Sorted entries: ("is_dir", 0/1), ("size", size). The const-pool
    // sorts by key bytes; "is_dir" < "size".
    let entries: [(&str, i64); 2] = [("is_dir", i64::from(is_dir)), ("size", size)];

    const HEADER_BYTES: u32 = 16;
    const ENTRY_BYTES: u32 = 16;
    let entry_count = entries.len() as u32;

    let record_off = relon_util::align_up(scratch_base.checked_add(pre_cursor)?, 8);

    let table_bytes = entry_count.checked_mul(ENTRY_BYTES)?;
    let key_payload_base = HEADER_BYTES.checked_add(table_bytes)?;
    let mut total = key_payload_base;
    for (k, _) in &entries {
        total = total.checked_add(u32::try_from(k.len()).ok()?)?;
    }
    let record_end = record_off.checked_add(total)?;
    if record_end > arena_len {
        return None;
    }

    let shape_hash = relon_ir::shape_hash::shape_hash_for_keys(entries.iter().map(|(k, _)| *k));

    let write_u32 = |rel: u32, val: u32| {
        // SAFETY: `[record_off, record_end)` is bounds-checked in-arena.
        unsafe {
            std::ptr::write_unaligned((arena_base + (record_off + rel) as usize) as *mut u32, val)
        };
    };
    let write_u64 = |rel: u32, val: u64| {
        // SAFETY: as above; record is 8-aligned so @8 / @value land
        // naturally.
        unsafe {
            std::ptr::write_unaligned((arena_base + (record_off + rel) as usize) as *mut u64, val)
        };
    };

    // Header.
    write_u32(0, entry_count);
    write_u32(4, 0); // pad
    write_u64(8, shape_hash);

    // Entry table + key payload. key_off is record-relative.
    let mut running_key_off = key_payload_base;
    let mut entry_rel = HEADER_BYTES;
    let mut key_rel = key_payload_base;
    for (k, v) in &entries {
        let klen = u32::try_from(k.len()).ok()?;
        write_u32(entry_rel, running_key_off);
        write_u32(entry_rel + 4, klen);
        write_u64(entry_rel + 8, *v as u64);
        // SAFETY: key payload `[key_rel, key_rel + klen)` is in-record.
        unsafe {
            std::ptr::copy_nonoverlapping(
                k.as_ptr(),
                (arena_base + (record_off + key_rel) as usize) as *mut u8,
                k.len(),
            );
        }
        running_key_off = running_key_off.checked_add(klen)?;
        entry_rel += ENTRY_BYTES;
        key_rel += klen;
    }

    let new_cursor = record_end.checked_sub(scratch_base)?;
    Some((record_off, new_cursor))
}

/// Record a trap reason into the sandbox so the codegen's negative-
/// return check can re-publish the typed [`crate::sandbox::RuntimeError`].
///
/// # Safety
///
/// `state` must be a live `SandboxState` (held by reference here).
unsafe fn record_trap(state: &SandboxState, kind: TrapKind) {
    // SAFETY: `raise_trap` only reads through the pointer to set the
    // trap code; the reference is valid for the call.
    unsafe {
        SandboxState::raise_trap(state as *const SandboxState, kind as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn write_record(buf: &mut [u8], off: usize, payload: &[u8]) -> usize {
        let len = payload.len() as u32;
        buf[off..off + 4].copy_from_slice(&len.to_le_bytes());
        buf[off + 4..off + 4 + payload.len()].copy_from_slice(payload);
        off + 4 + payload.len()
    }

    fn fixture_state(arena: &mut [u8]) -> Box<SandboxState> {
        let state = Box::new(SandboxState::new(Arc::new(
            crate::sandbox::CapabilityVtable::with_capacity(0),
        )));
        // SAFETY: `arena` outlives the test; we hold the only reference
        // to `state`.
        unsafe {
            state.install_arena(arena.as_mut_ptr(), arena.len() as u32);
        }
        state
    }

    /// Decode the `{is_dir, size}` dict record at `record_off`.
    fn decode_dict(arena: &[u8], record_off: usize) -> (bool, i64) {
        let entry_count =
            u32::from_le_bytes(arena[record_off..record_off + 4].try_into().unwrap()) as usize;
        let mut is_dir = None;
        let mut size = None;
        for i in 0..entry_count {
            let eoff = record_off + 16 + i * 16;
            let key_off =
                record_off + u32::from_le_bytes(arena[eoff..eoff + 4].try_into().unwrap()) as usize;
            let key_len =
                u32::from_le_bytes(arena[eoff + 4..eoff + 8].try_into().unwrap()) as usize;
            let value = i64::from_le_bytes(arena[eoff + 8..eoff + 16].try_into().unwrap());
            let key = std::str::from_utf8(&arena[key_off..key_off + key_len]).unwrap();
            match key {
                "is_dir" => is_dir = Some(value != 0),
                "size" => size = Some(value),
                other => panic!("unexpected key {other}"),
            }
        }
        (is_dir.unwrap(), size.unwrap())
    }

    #[test]
    fn stats_a_sandboxed_file() {
        let dir = std::env::temp_dir().join(format!(
            "relon_stat_helper_{}_{:p}",
            std::process::id(),
            &0u8
        ));
        std::fs::create_dir_all(&dir).unwrap();
        relon_util::set_fs_sandbox_root(&dir);
        std::fs::write(dir.join("data.txt"), b"hello").unwrap();

        let mut arena = vec![0u8; 8192];
        write_record(&mut arena, 0, b"data.txt");
        let state = fixture_state(&mut arena);
        unsafe { state.install_scratch_base(64) };

        let off = unsafe { relon_stat_helper(state.as_ref() as *const _, 0) };
        assert!(off >= 0, "helper returned failure sentinel {off}");
        let (is_dir, size) = decode_dict(&arena, off as usize);
        assert!(!is_dir, "regular file must not be a dir");
        assert_eq!(size, 5, "byte length of \"hello\"");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_a_sandboxed_dir() {
        let dir = std::env::temp_dir().join(format!(
            "relon_stat_helper_dir_{}_{:p}",
            std::process::id(),
            &1u8
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        relon_util::set_fs_sandbox_root(&dir);

        let mut arena = vec![0u8; 8192];
        write_record(&mut arena, 0, b"sub");
        let state = fixture_state(&mut arena);
        unsafe { state.install_scratch_base(64) };

        let off = unsafe { relon_stat_helper(state.as_ref() as *const _, 0) };
        assert!(off >= 0, "helper returned failure sentinel {off}");
        let (is_dir, _size) = decode_dict(&arena, off as usize);
        assert!(is_dir, "directory must report is_dir = true");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_escape() {
        let dir = std::env::temp_dir().join(format!(
            "relon_stat_helper_esc_{}_{:p}",
            std::process::id(),
            &2u8
        ));
        std::fs::create_dir_all(&dir).unwrap();
        relon_util::set_fs_sandbox_root(&dir);

        let mut arena = vec![0u8; 1024];
        write_record(&mut arena, 0, b"../escape");
        let state = fixture_state(&mut arena);
        unsafe { state.install_scratch_base(64) };

        let off = unsafe { relon_stat_helper(state.as_ref() as *const _, 0) };
        assert_eq!(off, STAT_FAILURE, "escape path must be refused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn null_state_returns_failure() {
        let off = unsafe { relon_stat_helper(std::ptr::null(), 0) };
        assert_eq!(off, STAT_FAILURE);
    }
}
