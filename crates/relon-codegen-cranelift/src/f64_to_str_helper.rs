//! Host helper backing the cranelift `Op::FloatToStr` lowering.
//!
//! Unlike `Op::IntToStr` (whose decimal digit loop is inlined as CLIF
//! ops), Float rendering reuses Rust's `f64` `Display` through the
//! shared `relon_ir::float_str::format_f64_display` core — the exact
//! byte producer the tree-walk oracle uses for `Value::Float`. The
//! codegen pass therefore emits an indirect host-helper call through
//! the [`crate::vtable::VtableSlot::RelonF64ToStr`] slot, hitting this
//! function. The LLVM-native and LLVM-wasm backends call the same
//! core, so a Display-algorithm drift between backends is impossible
//! by construction.
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn relon_f64_to_str_helper(
//!     state:    *const SandboxState,
//!     bits:     i64,   // IEEE-754 bit pattern of the f64 to render
//!     dest_off: i32,   // arena-relative offset of the scratch record
//! ) -> i32             // payload length, or -1 on failure
//! ```
//!
//! The destination is a scratch-arena record of
//! [`relon_ir::float_str::FLOAT_TO_STR_RECORD_SIZE`] bytes the codegen
//! pre-allocated (bump allocation + bounds check) before the call.
//! The helper writes the canonical `[len: u32 LE][utf8 payload]`
//! String record at `arena_base + dest_off` and returns the payload
//! length. A `-1` return signals a malformed destination (negative
//! offset, record walking past the arena end, null/missing arena) —
//! the codegen traps on any negative return so a half-written record
//! is never observable.

use relon_ir::float_str::{format_f64_display, FLOAT_TO_STR_MAX_PAYLOAD, FLOAT_TO_STR_RECORD_SIZE};

use crate::sandbox::SandboxState;

/// Cranelift-callable `Op::FloatToStr` leaf. Renders the `f64` whose
/// bit pattern is `bits` into the pre-allocated scratch record at
/// `dest_off` inside the arena owned by `state`.
///
/// # Safety
///
/// * `state` must point at a live, properly aligned [`SandboxState`]
///   with an arena installed via [`SandboxState::install_arena`].
/// * `dest_off` must be the arena-relative offset of a scratch
///   allocation of at least [`FLOAT_TO_STR_RECORD_SIZE`] bytes that
///   the emitted code's bump allocator produced (its bounds check
///   guarantees the whole record lies inside the arena); the helper
///   re-validates against `arena_len` as defence-in-depth and returns
///   `-1` instead of writing out of bounds.
pub(crate) unsafe extern "C" fn relon_f64_to_str_helper(
    state: *const SandboxState,
    bits: i64,
    dest_off: i32,
) -> i32 {
    if state.is_null() || dest_off < 0 {
        return -1;
    }
    // SAFETY: the caller upholds the SandboxState invariants per the
    // module-level safety contract.
    let state_ref = unsafe { &*state };
    let arena_base = state_ref.arena_base();
    let arena_len = state_ref.arena_len();
    if arena_base == 0 {
        return -1;
    }
    // The whole record allocation must lie inside the arena.
    let record_end = match (dest_off as u32).checked_add(FLOAT_TO_STR_RECORD_SIZE) {
        Some(end) => end,
        None => return -1,
    };
    if record_end > arena_len {
        return -1;
    }

    let mut payload = [0u8; FLOAT_TO_STR_MAX_PAYLOAD];
    let len = match format_f64_display(bits as u64, &mut payload) {
        Some(len) => len,
        // Cannot happen for FLOAT_TO_STR_MAX_PAYLOAD-sized buffers
        // (audited bound 327 < 352); refuse loudly if it ever does.
        None => return -1,
    };

    let dest = (arena_base + dest_off as usize) as *mut u8;
    // SAFETY: `[dest, dest + 4 + len)` lies inside the arena —
    // `record_end <= arena_len` above and
    // `4 + len <= 4 + FLOAT_TO_STR_MAX_PAYLOAD <= FLOAT_TO_STR_RECORD_SIZE`
    // (asserted in `relon_ir::float_str` tests).
    unsafe {
        std::ptr::write_unaligned(dest as *mut u32, len as u32);
        std::ptr::copy_nonoverlapping(payload.as_ptr(), dest.add(4), len);
    }
    len as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocate a `SandboxState` and install the arena. Mirrors the
    /// `glob_helper` test fixture.
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

    fn render_via_helper(v: f64) -> String {
        let mut arena = vec![0u8; FLOAT_TO_STR_RECORD_SIZE as usize];
        let state = fixture_state(&mut arena);
        // SAFETY: state outlives the call; offset 0 + RECORD_SIZE fits
        // the arena exactly.
        let len =
            unsafe { relon_f64_to_str_helper(state.as_ref() as *const _, v.to_bits() as i64, 0) };
        assert!(len >= 0, "helper refused a valid destination for {v:?}");
        let header = u32::from_le_bytes(arena[0..4].try_into().unwrap());
        assert_eq!(header as i32, len, "header/return length drift");
        String::from_utf8(arena[4..4 + len as usize].to_vec()).expect("utf8")
    }

    #[test]
    fn renders_display_bytes_for_boundary_battery() {
        for v in [
            1.0,
            -0.0,
            0.1,
            567.34,
            1e300,
            5e-324,
            -5e-324,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert_eq!(render_via_helper(v), format!("{v}"), "drift for {v:?}");
        }
    }

    #[test]
    fn null_state_returns_negative() {
        let r = unsafe { relon_f64_to_str_helper(std::ptr::null(), 0, 0) };
        assert_eq!(r, -1);
    }

    #[test]
    fn negative_offset_returns_negative() {
        let mut arena = vec![0u8; FLOAT_TO_STR_RECORD_SIZE as usize];
        let state = fixture_state(&mut arena);
        let r = unsafe { relon_f64_to_str_helper(state.as_ref() as *const _, 0, -8) };
        assert_eq!(r, -1);
    }

    #[test]
    fn record_past_arena_end_returns_negative() {
        // Arena shorter than one record: any offset overflows.
        let mut arena = vec![0u8; 64];
        let state = fixture_state(&mut arena);
        let r = unsafe { relon_f64_to_str_helper(state.as_ref() as *const _, 0, 0) };
        assert_eq!(r, -1);
    }
}
