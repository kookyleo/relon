//! Host helper backing the cranelift `Op::ReadDir` lowering — the
//! built-in `read_dir(path) -> List<String>` primitive (P-fs Stage 2).
//!
//! The cranelift codegen lowers `Op::ReadDir` to an indirect call
//! through the [`crate::vtable::VtableSlot::RelonReadDir`] slot. The
//! capability gate (`reads_fs`) already fired in the preceding
//! `Op::CheckCap`.
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn relon_read_dir_helper(
//!     state:    *const SandboxState,
//!     path_off: i32,   // arena-relative offset of the path String record
//! ) -> i32             // arena-relative offset of the List<String>
//!                      // header record, or a negative sentinel on failure
//! ```
//!
//! The input path is a wasm-style String record `[len: u32 LE][utf8
//! bytes]` at `arena_base + path_off`. The helper:
//!
//!   1. reads the path bytes out of the arena (bounds-checked);
//!   2. resolves the path against the shared filesystem sandbox root
//!      (`relon_util::resolve_fs_sandbox_path`) — refusing escapes;
//!   3. lists the directory's entry file names (bare names, no path
//!      prefix; non-UTF-8 names are skipped);
//!   4. **sorts** the names byte-lexicographically — `std::fs::read_dir`
//!      iteration order is OS-unspecified, and the sort is what keeps
//!      the cranelift / llvm-native / tree-walk listings bit-identical;
//!   5. bump-allocates a `List<String>` pointer-array record in the
//!      scratch region (the same layout `Op::ConstListString` emits:
//!      element String records `[slen][utf8]` first, each 4-aligned,
//!      then a `[len][off_0]...[off_{N-1}]` header whose `off_i` is the
//!      arena-relative offset of String record `i`), and returns the
//!      header's arena-relative offset.
//!
//! On any failure (malformed path record, sandbox escape, I/O error,
//! arena overflow) the helper records a [`TrapKind`] into the sandbox
//! trap slot and returns a negative sentinel; the codegen turns the
//! negative result into the standard trap epilogue.

use crate::sandbox::{SandboxState, TrapKind};

/// Negative sentinel the codegen checks for: any negative return means
/// "the listing failed; the trap slot holds the reason".
const READ_DIR_FAILURE: i32 = -1;

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

/// Cranelift-callable helper for `Op::ReadDir`.
///
/// # Safety
///
/// * `state` must point at a live, properly aligned [`SandboxState`]
///   with an arena installed via [`SandboxState::install_arena`].
/// * `path_off` must be an arena-relative offset the codegen produced
///   for an `IrType::String` value.
pub(crate) unsafe extern "C" fn relon_read_dir_helper(
    state: *const SandboxState,
    path_off: i32,
) -> i32 {
    if state.is_null() {
        return READ_DIR_FAILURE;
    }
    // SAFETY: the caller upholds the SandboxState invariants.
    let state_ref = unsafe { &*state };
    let arena_base = state_ref.arena_base();
    let arena_len = state_ref.arena_len();
    if arena_base == 0 {
        unsafe { record_trap(state_ref, TrapKind::Unreachable) };
        return READ_DIR_FAILURE;
    }

    // 1. Read the path bytes out of the arena.
    let (path_len, path_ptr) = match unsafe { read_record(arena_base, arena_len, path_off) } {
        Some(v) => v,
        None => {
            unsafe { record_trap(state_ref, TrapKind::Unreachable) };
            return READ_DIR_FAILURE;
        }
    };
    // SAFETY: `read_record` validated the payload range.
    let path_bytes = unsafe { std::slice::from_raw_parts(path_ptr, path_len as usize) };
    let path = match std::str::from_utf8(path_bytes) {
        Ok(v) => v,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::Unreachable) };
            return READ_DIR_FAILURE;
        }
    };

    // 2. Resolve against the sandbox root (refuses escapes).
    let resolved = match relon_util::resolve_fs_sandbox_path(path) {
        Ok(p) => p,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::CapabilityDenied) };
            return READ_DIR_FAILURE;
        }
    };

    // 3. List the directory + 4. sort the names.
    let names = match list_sorted_names(&resolved) {
        Some(v) => v,
        None => {
            unsafe { record_trap(state_ref, TrapKind::CapabilityDenied) };
            return READ_DIR_FAILURE;
        }
    };

    // 5. Bump-allocate the List<String> record into the scratch region.
    let scratch_base = unsafe { state_ref.scratch_base() };
    let pre_cursor = unsafe { state_ref.scratch_cursor() };
    match unsafe {
        materialize_list_string(arena_base, arena_len, scratch_base, pre_cursor, &names)
    } {
        Some((header_off, new_cursor)) => {
            unsafe { state_ref.set_scratch_cursor(new_cursor) };
            match i32::try_from(header_off) {
                Ok(v) => v,
                Err(_) => {
                    unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
                    READ_DIR_FAILURE
                }
            }
        }
        None => {
            unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
            READ_DIR_FAILURE
        }
    }
}

/// List a directory's entry file names (bare names, no prefix), skipping
/// non-UTF-8 names, and return them sorted byte-lexicographically.
/// Returns `None` on an I/O error (the directory does not exist, is not
/// a directory, or is unreadable).
fn list_sorted_names(dir: &std::path::Path) -> Option<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    names.sort_unstable();
    Some(names)
}

/// Write a sorted `List<String>` pointer-array record into the scratch
/// region at `scratch_base + pre_cursor` (4-aligned), mirroring the
/// `Op::ConstListString` arena layout. Returns `(header_off,
/// new_scratch_cursor)` on success (both arena-relative for the offset,
/// scratch-relative for the cursor), or `None` on arena overflow / u32
/// overflow.
///
/// # Safety
///
/// `[arena_base, arena_base + arena_len)` must be a live, host-owned,
/// writable arena segment for the duration of the call.
unsafe fn materialize_list_string(
    arena_base: usize,
    arena_len: u32,
    scratch_base: u32,
    pre_cursor: u32,
    names: &[String],
) -> Option<(u32, u32)> {
    // Element String records first, each 4-aligned, capturing each
    // record's arena-relative offset.
    let mut cursor = relon_util::align_up(scratch_base.checked_add(pre_cursor)?, 4);
    let mut str_offsets: Vec<u32> = Vec::with_capacity(names.len());
    for name in names {
        cursor = relon_util::align_up(cursor, 4);
        let slen = u32::try_from(name.len()).ok()?;
        let header_end = cursor.checked_add(4)?;
        let record_end = header_end.checked_add(slen)?;
        if record_end > arena_len {
            return None;
        }
        // SAFETY: the record `[cursor, record_end)` lies inside the
        // arena (bounds checked above); we own the arena here.
        unsafe {
            std::ptr::write_unaligned((arena_base + cursor as usize) as *mut u32, slen);
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                (arena_base + header_end as usize) as *mut u8,
                name.len(),
            );
        }
        str_offsets.push(cursor);
        cursor = record_end;
    }

    // Header `[len][off_0]...[off_{N-1}]`, 4-aligned.
    cursor = relon_util::align_up(cursor, 4);
    let header_off = cursor;
    let len = u32::try_from(names.len()).ok()?;
    let header_end = cursor.checked_add(4)?;
    let offsets_end = header_end.checked_add(len.checked_mul(4)?)?;
    if offsets_end > arena_len {
        return None;
    }
    // SAFETY: `[header_off, offsets_end)` is inside the arena.
    unsafe {
        std::ptr::write_unaligned((arena_base + header_off as usize) as *mut u32, len);
        let mut w = header_end;
        for off in &str_offsets {
            std::ptr::write_unaligned((arena_base + w as usize) as *mut u32, *off);
            w += 4;
        }
    }

    let new_cursor = offsets_end.checked_sub(scratch_base)?;
    Some((header_off, new_cursor))
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

    /// Decode a List<String> record at `header_off` into a Vec<String>.
    fn decode_list(arena: &[u8], header_off: usize) -> Vec<String> {
        let len =
            u32::from_le_bytes(arena[header_off..header_off + 4].try_into().unwrap()) as usize;
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let off_pos = header_off + 4 + i * 4;
            let s_off =
                u32::from_le_bytes(arena[off_pos..off_pos + 4].try_into().unwrap()) as usize;
            let slen = u32::from_le_bytes(arena[s_off..s_off + 4].try_into().unwrap()) as usize;
            out.push(String::from_utf8(arena[s_off + 4..s_off + 4 + slen].to_vec()).unwrap());
        }
        out
    }

    #[test]
    fn lists_a_sandboxed_dir_sorted() {
        let dir =
            std::env::temp_dir().join(format!("relon_rd_helper_{}_{:p}", std::process::id(), &0u8));
        std::fs::create_dir_all(&dir).unwrap();
        relon_util::set_fs_sandbox_root(&dir);
        // Create out-of-order so the sort is observable.
        std::fs::write(dir.join("zeta.txt"), "z").unwrap();
        std::fs::write(dir.join("alpha.txt"), "a").unwrap();
        std::fs::write(dir.join("middle.txt"), "m").unwrap();

        let mut arena = vec![0u8; 8192];
        // Path record "." (the sandbox root itself) at offset 0.
        write_record(&mut arena, 0, b".");
        let state = fixture_state(&mut arena);
        unsafe { state.install_scratch_base(64) };

        let off = unsafe { relon_read_dir_helper(state.as_ref() as *const _, 0) };
        assert!(off >= 0, "helper returned failure sentinel {off}");
        let names = decode_list(&arena, off as usize);
        assert_eq!(names, vec!["alpha.txt", "middle.txt", "zeta.txt"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_escape() {
        let dir = std::env::temp_dir().join(format!(
            "relon_rd_helper_esc_{}_{:p}",
            std::process::id(),
            &1u8
        ));
        std::fs::create_dir_all(&dir).unwrap();
        relon_util::set_fs_sandbox_root(&dir);

        let mut arena = vec![0u8; 1024];
        write_record(&mut arena, 0, b"../escape");
        let state = fixture_state(&mut arena);
        unsafe { state.install_scratch_base(64) };

        let off = unsafe { relon_read_dir_helper(state.as_ref() as *const _, 0) };
        assert_eq!(off, READ_DIR_FAILURE, "escape path must be refused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn null_state_returns_failure() {
        let off = unsafe { relon_read_dir_helper(std::ptr::null(), 0) };
        assert_eq!(off, READ_DIR_FAILURE);
    }
}
