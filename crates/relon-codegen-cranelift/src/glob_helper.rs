//! Host helper backing the cranelift `Op::Call { fn_index ==
//! GLOB_MATCH_INDEX }` interception.
//!
//! The cranelift codegen for the bundled stdlib's `glob_match(s,
//! pattern) -> Bool` body **does not** inline an IR-op transcription
//! of the matcher (writing the full Unicode glob algorithm out as
//! IR ops would balloon the bundle by ~1.5kloc with no real
//! benefit). Instead, `emit_call_stdlib` intercepts the call and
//! emits an indirect host-helper call through the
//! [`crate::vtable::VtableSlot::RelonGlobMatch`] slot, hitting this
//! function.
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn relon_glob_match_helper(
//!     state: *const SandboxState,
//!     s_off:  i32,   // arena-relative offset of the haystack String record
//!     p_off:  i32,   // arena-relative offset of the pattern String record
//! ) -> i32           // 1 = match, 0 = no-match
//! ```
//!
//! Both `String` records use the canonical layout the rest of the
//! cranelift backend speaks: `[len: u32 LE][utf8 bytes]`. The helper
//! resolves the absolute address as `arena_base + s_off`, reads the
//! 4-byte length header, validates the UTF-8 payload, and runs the
//! shared linear-time matcher from `relon_ir::glob::glob_match`.
//!
//! On any malformed input (invalid UTF-8 in either operand, header
//! overflow against the arena length) the helper returns `0` (no
//! match) rather than tripping a host-side panic — the arena bounds
//! check in the codegen ahead of the call already guarantees that
//! the header bytes are in-range, so a UTF-8 validation failure is
//! treated as the same "this string can never match a Unicode glob"
//! signal a stale arena would produce.

use crate::sandbox::SandboxState;

/// Read the `(len, payload_addr)` of a wasm-style String record at
/// `arena_base + off`. Returns `None` when the header / payload walks
/// past `arena_base + arena_len` so the caller can refuse the match
/// rather than panic.
///
/// # Safety
///
/// `arena_base` + `arena_len` must describe a live, host-owned arena
/// segment. The codegen pass populates these from
/// [`SandboxState::install_arena`] before invoking the helper.
unsafe fn read_record(arena_base: usize, arena_len: u32, off: i32) -> Option<(u32, *const u8)> {
    // Reject negative offsets eagerly — i32 arena offsets are always
    // non-negative on the supported codegen surface; a negative value
    // signals stale state and must surface as "no match" rather than
    // panic.
    if off < 0 {
        return None;
    }
    let off_u = off as u32;
    // The 4-byte length header must lie entirely inside the arena.
    let header_end = off_u.checked_add(4)?;
    if header_end > arena_len {
        return None;
    }
    let header_addr = arena_base.checked_add(off_u as usize)?;
    // SAFETY: the codegen-side arena bounds check guarantees the
    // header is in-range, and the host caller upholds the arena
    // invariants per the module-level safety contract.
    let len_bytes = unsafe { std::ptr::read_unaligned(header_addr as *const u32) };
    let payload_end = header_end.checked_add(len_bytes)?;
    if payload_end > arena_len {
        return None;
    }
    let payload_addr = arena_base.checked_add((off_u + 4) as usize)?;
    Some((len_bytes, payload_addr as *const u8))
}

/// Cranelift-callable helper that runs [`relon_ir::glob::glob_match`]
/// against the two String records at `s_off` / `p_off` inside the
/// arena owned by `state`.
///
/// # Safety
///
/// * `state` must point at a live, properly aligned [`SandboxState`]
///   with an arena installed via [`SandboxState::install_arena`].
/// * Both `s_off` and `p_off` must be arena-relative offsets the
///   codegen pass produced for `IrType::String` values; the codegen
///   bounds check guarantees the leading 4-byte length header lies
///   inside the arena.
pub(crate) unsafe extern "C" fn relon_glob_match_helper(
    state: *const SandboxState,
    s_off: i32,
    p_off: i32,
) -> i32 {
    if state.is_null() {
        return 0;
    }
    // SAFETY: the caller upholds the SandboxState invariants per the
    // module-level safety contract.
    let state_ref = unsafe { &*state };
    let arena_base = state_ref.arena_base();
    let arena_len = state_ref.arena_len();
    if arena_base == 0 {
        return 0;
    }

    // SAFETY: `read_record` upholds its own bounds invariants against
    // `arena_len` before producing the payload pointer; the codegen
    // bounds check ahead of the call further guarantees the header
    // load is safe.
    let (s_len, s_ptr) = match unsafe { read_record(arena_base, arena_len, s_off) } {
        Some(v) => v,
        None => return 0,
    };
    let (p_len, p_ptr) = match unsafe { read_record(arena_base, arena_len, p_off) } {
        Some(v) => v,
        None => return 0,
    };

    // SAFETY: `read_record` validated that `[s_ptr, s_ptr + s_len)`
    // and `[p_ptr, p_ptr + p_len)` lie inside the arena.
    let s_bytes = unsafe { std::slice::from_raw_parts(s_ptr, s_len as usize) };
    let p_bytes = unsafe { std::slice::from_raw_parts(p_ptr, p_len as usize) };

    // Reject invalid UTF-8 as "no match" — both operands are declared
    // as `String` in the IR, so the codegen-side type check already
    // guarantees a well-formed payload on the supported surface; this
    // is defence-in-depth against arena tampering rather than a
    // user-visible behaviour.
    let s = match std::str::from_utf8(s_bytes) {
        Ok(v) => v,
        Err(_) => return 0,
    };
    let pattern = match std::str::from_utf8(p_bytes) {
        Ok(v) => v,
        Err(_) => return 0,
    };

    if relon_ir::glob::glob_match(s, pattern) {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a wasm-style String record `[len: u32 LE][bytes]` at the
    /// start of `buf`, return the byte length. Helper for the
    /// end-to-end tests below.
    fn write_record(buf: &mut [u8], payload: &[u8]) -> u32 {
        let len = payload.len() as u32;
        buf[0..4].copy_from_slice(&len.to_le_bytes());
        buf[4..4 + payload.len()].copy_from_slice(payload);
        4 + len
    }

    /// Build a small arena: two String records back-to-back. Returns
    /// the offsets of each record.
    fn build_two_records(s: &[u8], p: &[u8]) -> (Vec<u8>, i32, i32) {
        let cap = 4 + s.len() + 4 + p.len() + 16;
        let mut buf = vec![0u8; cap];
        let s_off = 0i32;
        let bytes_written = write_record(&mut buf, s);
        let p_off = bytes_written as i32;
        write_record(&mut buf[bytes_written as usize..], p);
        (buf, s_off, p_off)
    }

    /// Allocate a `SandboxState` and install the arena. Returns the
    /// state (boxed so the pointer stays stable) and the arena buffer
    /// (kept alive by the caller).
    fn fixture_state(arena: &mut Vec<u8>) -> Box<SandboxState> {
        let state = Box::new(SandboxState::new(std::sync::Arc::new(
            crate::sandbox::CapabilityVtable::with_capacity(0),
        )));
        // SAFETY: `arena` outlives the test (lives in the caller's
        // stack frame) and we hold the only reference to `state`
        // before any helper invocation.
        unsafe {
            state.install_arena(arena.as_mut_ptr(), arena.len() as u32);
        }
        state
    }

    #[test]
    fn matches_simple_glob_through_helper() {
        let (mut arena, s_off, p_off) = build_two_records(b"hello world", b"hello *");
        let state = fixture_state(&mut arena);
        // SAFETY: state outlives the call; offsets were produced by
        // `build_two_records` which respects the arena layout.
        let result = unsafe { relon_glob_match_helper(state.as_ref() as *const _, s_off, p_off) };
        assert_eq!(result, 1);
    }

    #[test]
    fn rejects_non_matching_glob_through_helper() {
        let (mut arena, s_off, p_off) = build_two_records(b"hello world", b"goodbye *");
        let state = fixture_state(&mut arena);
        let result = unsafe { relon_glob_match_helper(state.as_ref() as *const _, s_off, p_off) };
        assert_eq!(result, 0);
    }

    #[test]
    fn handles_empty_string_against_star() {
        let (mut arena, s_off, p_off) = build_two_records(b"", b"*");
        let state = fixture_state(&mut arena);
        let result = unsafe { relon_glob_match_helper(state.as_ref() as *const _, s_off, p_off) };
        assert_eq!(result, 1);
    }

    #[test]
    fn null_state_returns_no_match() {
        let result = unsafe { relon_glob_match_helper(std::ptr::null(), 0, 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn out_of_arena_offset_returns_no_match() {
        let mut arena = vec![0u8; 32];
        let state = fixture_state(&mut arena);
        // `s_off = 1024` walks far past `arena_len = 32`; the helper
        // must refuse rather than read past the arena.
        let result = unsafe { relon_glob_match_helper(state.as_ref() as *const _, 1024, 0) };
        assert_eq!(result, 0);
    }

    #[test]
    fn unicode_helper_matches_multi_byte_codepoints() {
        // 4-byte UTF-8 emoji + 2-byte UTF-8 Greek + ASCII mixed.
        let (mut arena, s_off, p_off) = build_two_records("αβγ🦀".as_bytes(), "α*🦀".as_bytes());
        let state = fixture_state(&mut arena);
        let result = unsafe { relon_glob_match_helper(state.as_ref() as *const _, s_off, p_off) };
        assert_eq!(result, 1);
    }
}
