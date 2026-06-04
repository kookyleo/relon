//! Host helper backing the cranelift `Op::ReadFile` lowering — the
//! built-in `read_file(path) -> String` primitive (P-fs Stage 1).
//!
//! The cranelift codegen lowers `Op::ReadFile` to an indirect call
//! through the [`crate::vtable::VtableSlot::RelonReadFile`] slot. The
//! capability gate (`reads_fs`) already fired in the preceding
//! `Op::CheckCap`.
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn relon_read_file_helper(
//!     state:    *const SandboxState,
//!     path_off: i32,   // arena-relative offset of the path String record
//! ) -> i32             // arena-relative offset of the contents String
//!                      // record, or a negative sentinel on failure
//! ```
//!
//! The input path is a wasm-style String record `[len: u32 LE][utf8
//! bytes]` at `arena_base + path_off`, the same layout the rest of the
//! cranelift backend speaks. The helper:
//!
//!   1. reads the path bytes out of the arena (bounds-checked against
//!      `arena_len`);
//!   2. resolves the path against the shared filesystem sandbox root
//!      (`relon_util::resolve_fs_sandbox_path`) — refusing any path that
//!      escapes the root, mirroring the wasm preopen-dir model;
//!   3. reads the file's bytes;
//!   4. bump-allocates a fresh String record at the current
//!      `tail_cursor` (aligned to 4, the String record alignment),
//!      writes `[len][bytes]`, advances `tail_cursor`, and returns the
//!      record's arena-relative offset.
//!
//! On any failure (malformed path record, sandbox escape, I/O error,
//! arena overflow) the helper records a [`TrapKind`] into the sandbox
//! trap slot and returns a negative sentinel; the codegen turns the
//! negative result into the standard trap epilogue.

use crate::sandbox::{SandboxState, TrapKind};

/// Negative sentinel the codegen checks for: any negative return means
/// "the read failed; the trap slot holds the reason".
const READ_FILE_FAILURE: i32 = -1;

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

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up(value: u32, align: u32) -> u32 {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

/// Cranelift-callable helper for `Op::ReadFile`.
///
/// # Safety
///
/// * `state` must point at a live, properly aligned [`SandboxState`]
///   with an arena installed via [`SandboxState::install_arena`].
/// * `path_off` must be an arena-relative offset the codegen produced
///   for an `IrType::String` value.
pub(crate) unsafe extern "C" fn relon_read_file_helper(
    state: *const SandboxState,
    path_off: i32,
) -> i32 {
    if state.is_null() {
        return READ_FILE_FAILURE;
    }
    // SAFETY: the caller upholds the SandboxState invariants.
    let state_ref = unsafe { &*state };
    let arena_base = state_ref.arena_base();
    let arena_len = state_ref.arena_len();
    if arena_base == 0 {
        unsafe { record_trap(state_ref, TrapKind::Unreachable) };
        return READ_FILE_FAILURE;
    }

    // 1. Read the path bytes out of the arena.
    let (path_len, path_ptr) = match unsafe { read_record(arena_base, arena_len, path_off) } {
        Some(v) => v,
        None => {
            unsafe { record_trap(state_ref, TrapKind::Unreachable) };
            return READ_FILE_FAILURE;
        }
    };
    // SAFETY: `read_record` validated the payload range.
    let path_bytes = unsafe { std::slice::from_raw_parts(path_ptr, path_len as usize) };
    let path = match std::str::from_utf8(path_bytes) {
        Ok(v) => v,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::Unreachable) };
            return READ_FILE_FAILURE;
        }
    };

    // 2. Resolve against the sandbox root (refuses escapes).
    let resolved = match relon_util::resolve_fs_sandbox_path(path) {
        Ok(p) => p,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::CapabilityDenied) };
            return READ_FILE_FAILURE;
        }
    };

    // 3. Read the file.
    let contents = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::CapabilityDenied) };
            return READ_FILE_FAILURE;
        }
    };
    let content_len = match u32::try_from(contents.len()) {
        Ok(n) => n,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
            return READ_FILE_FAILURE;
        }
    };

    // 4. Bump-allocate a fresh String record `[len][bytes]` inside the
    //    scratch region (`scratch_base + scratch_cursor`), 4-aligned —
    //    the same arena-relative offset convention the codegen's
    //    `Op::AllocScratch` path produces for String operands, so the
    //    record resolves verbatim as a `String` value on the operand
    //    stack and the return-store path copies it out normally.
    let scratch_base = unsafe { state_ref.scratch_base() };
    let pre_cursor = unsafe { state_ref.scratch_cursor() };
    let aligned_cursor = align_up(pre_cursor, 4);
    let record_off = match scratch_base.checked_add(aligned_cursor) {
        Some(v) => v,
        None => {
            unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
            return READ_FILE_FAILURE;
        }
    };
    let header_end = match record_off.checked_add(4) {
        Some(v) => v,
        None => {
            unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
            return READ_FILE_FAILURE;
        }
    };
    let record_end = match header_end.checked_add(content_len) {
        Some(v) => v,
        None => {
            unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
            return READ_FILE_FAILURE;
        }
    };
    if record_end > arena_len {
        // Not enough room in the scratch region for the file contents.
        unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
        return READ_FILE_FAILURE;
    }

    // SAFETY: the record `[record_off, record_end)` lies inside the
    // arena (bounds checked above); we own the arena for the duration
    // of the helper call.
    unsafe {
        let header_addr = (arena_base + record_off as usize) as *mut u32;
        std::ptr::write_unaligned(header_addr, content_len);
        let payload_addr = (arena_base + header_end as usize) as *mut u8;
        std::ptr::copy_nonoverlapping(contents.as_ptr(), payload_addr, contents.len());
        // Publish the post-bump scratch cursor (relative to scratch_base).
        state_ref.set_scratch_cursor(record_end - scratch_base);
    }

    // Arena offsets stay within i32 range on the supported surface
    // (arena_len is u32 and bounded well under i32::MAX in practice).
    match i32::try_from(record_off) {
        Ok(v) => v,
        Err(_) => {
            unsafe { record_trap(state_ref, TrapKind::ResourceExhausted) };
            READ_FILE_FAILURE
        }
    }
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

    #[test]
    fn reads_a_sandboxed_file_into_a_tail_record() {
        let dir =
            std::env::temp_dir().join(format!("relon_rf_helper_{}_{:p}", std::process::id(), &0u8));
        std::fs::create_dir_all(&dir).unwrap();
        relon_util::set_fs_sandbox_root(&dir);
        const CONTENT: &str = "hello read_file helper\n";
        let name = "fixture.txt";
        std::fs::write(dir.join(name), CONTENT).unwrap();

        // Arena: path record at offset 0, plenty of tail room.
        let mut arena = vec![0u8; 4096];
        write_record(&mut arena, 0, name.as_bytes());
        let state = fixture_state(&mut arena);
        // Scratch region starts past the path record.
        unsafe { state.install_scratch_base(64) };

        let off = unsafe { relon_read_file_helper(state.as_ref() as *const _, 0) };
        assert!(off >= 0, "helper returned failure sentinel {off}");

        // Decode the returned String record out of the arena.
        let off = off as usize;
        let len = u32::from_le_bytes(arena[off..off + 4].try_into().unwrap()) as usize;
        let got = &arena[off + 4..off + 4 + len];
        assert_eq!(got, CONTENT.as_bytes());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_escape() {
        let dir = std::env::temp_dir().join(format!(
            "relon_rf_helper_esc_{}_{:p}",
            std::process::id(),
            &1u8
        ));
        std::fs::create_dir_all(&dir).unwrap();
        relon_util::set_fs_sandbox_root(&dir);

        let mut arena = vec![0u8; 1024];
        write_record(&mut arena, 0, b"../escape.txt");
        let state = fixture_state(&mut arena);
        unsafe { state.install_scratch_base(64) };

        let off = unsafe { relon_read_file_helper(state.as_ref() as *const _, 0) };
        assert_eq!(off, READ_FILE_FAILURE, "escape path must be refused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn null_state_returns_failure() {
        let off = unsafe { relon_read_file_helper(std::ptr::null(), 0) };
        assert_eq!(off, READ_FILE_FAILURE);
    }
}
