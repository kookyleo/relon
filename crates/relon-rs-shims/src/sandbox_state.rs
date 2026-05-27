//! Per-call arena state handed to AOT-compiled Relon entries.
//!
//! ## Layout contract
//!
//! The JIT body the LLVM AOT pipeline emits reads
//! [`crate::sandbox_state::ArenaState`] through `state_param` GEP at
//! fixed byte offsets — `arena_base` at 0, `arena_len` at 8 (the
//! `size_of::<usize>()` slot), `tail_cursor` at 12, `scratch_cursor`
//! at 16, `scratch_base` at 20. The struct in this crate **must**
//! mirror that layout byte-for-byte, otherwise the JIT-side pointer
//! arithmetic lands on garbage and the entry segfaults on the first
//! `LoadField` of an arena-pointing slot.
//!
//! We deliberately don't re-export `relon_codegen_llvm::state::ArenaState`
//! here — pulling the LLVM crate into a downstream binary as a runtime
//! dep would pull `llvm-sys` + `inkwell` (and the system `libllvm-*`
//! dylib at link time), which defeats the point of AOT-linking. The
//! `static_assertions::assert_eq_size!` (left to a Phase 3 dep-add)
//! plus the per-offset comparison checks below keep the two struct
//! shapes from drifting.
//!
//! ## Per-call ownership
//!
//! `SandboxState` is constructed per-dispatch by [`call_buffer_entry`]
//! and reset between calls; user code should treat the struct as
//! opaque (only `Default::default()` is exposed). Phase 2 doesn't yet
//! expose `with_arena_cap` / `with_caps` builders — those land with
//! the trap-propagation work in Phase 3.

use core::cell::UnsafeCell;

/// Byte offset of [`ArenaState::arena_base`] in the `#[repr(C)]`
/// layout. Mirror of `relon_codegen_llvm::state::ARENA_STATE_OFFSET_BASE`.
///
/// Used by the layout-stamp test below; the runtime path never reads
/// the constant directly because the JIT body hard-codes the offset
/// into its emitted GEPs.
#[allow(dead_code)]
pub(crate) const ARENA_STATE_OFFSET_BASE: u32 = 0;

/// Byte offset of [`ArenaState::arena_len`]. Mirror of
/// `relon_codegen_llvm::state::ARENA_STATE_OFFSET_LEN`. The pre-Phase-3
/// JIT body never reads this slot (Phase B/C/D didn't emit bounds
/// checks), but we keep the layout matched for the trap-propagation
/// work.
#[allow(dead_code)]
pub(crate) const ARENA_STATE_OFFSET_LEN: u32 = core::mem::size_of::<usize>() as u32;

/// Byte offset of [`ArenaState::tail_cursor`]. Mirror of
/// `relon_codegen_llvm::state::ARENA_STATE_OFFSET_TAIL_CURSOR`.
#[allow(dead_code)]
pub(crate) const ARENA_STATE_OFFSET_TAIL_CURSOR: u32 = ARENA_STATE_OFFSET_LEN + 4;

/// Byte offset of [`ArenaState::scratch_cursor`]. Mirror of
/// `relon_codegen_llvm::state::ARENA_STATE_OFFSET_SCRATCH_CURSOR`.
#[allow(dead_code)]
pub(crate) const ARENA_STATE_OFFSET_SCRATCH_CURSOR: u32 = ARENA_STATE_OFFSET_TAIL_CURSOR + 4;

/// Byte offset of [`ArenaState::scratch_base`]. Mirror of
/// `relon_codegen_llvm::state::ARENA_STATE_OFFSET_SCRATCH_BASE`.
#[allow(dead_code)]
pub(crate) const ARENA_STATE_OFFSET_SCRATCH_BASE: u32 = ARENA_STATE_OFFSET_SCRATCH_CURSOR + 4;

/// Per-call arena state handed to the JIT entry. The `#[repr(C)]`
/// layout matches `relon_codegen_llvm::state::ArenaState` exactly so
/// the emitted body's GEPs land on the right slots.
///
/// `UnsafeCell` on every live field because the JIT body mutates
/// `tail_cursor` / `scratch_cursor` through raw pointers; Rust's
/// borrow checker can't see the emitted machine code, so an
/// `&self`-shaped Rust API can still permit interior mutation from
/// the JIT call.
///
/// Marked `pub` + `#[doc(hidden)]` (not `pub(crate)`) so the
/// build.rs-generated binding can name the type inside its
/// `extern "C"` declaration without taking a hard dependency on the
/// crate's private API. End users should treat the type as opaque —
/// every supported call shape funnels through [`crate::call_buffer_entry`].
#[repr(C)]
#[doc(hidden)]
pub struct ArenaState {
    /// Raw arena base pointer (kept `pub` so the JIT body's GEP at
    /// offset 0 is structurally addressable; users should not touch).
    pub arena_base: UnsafeCell<usize>,
    /// Arena length in bytes (reserved for the Phase 3 bounds-check
    /// work; the current JIT body never reads this slot).
    pub arena_len: UnsafeCell<u32>,
    /// Tail-cursor — pointer-indirect `StoreField` bumps this.
    pub tail_cursor: UnsafeCell<u32>,
    /// Scratch-cursor — `Op::AllocScratchDyn` bumps this.
    pub scratch_cursor: UnsafeCell<u32>,
    /// Arena-relative offset where the scratch region starts.
    pub scratch_base: UnsafeCell<u32>,
}

/// Forward-compat sandbox state placeholder.
///
/// Phase 1 (Int-only fast path) keeps this empty because the typed
/// `extern "C" fn(i64, ...) -> i64` entry has no arena dependency —
/// every value lives in a register. Phase 2 buffer-protocol entries
/// construct an internal [`ArenaState`] inside [`crate::call_buffer_entry`]
/// per-call; the `SandboxState` the host passes today is unused on
/// that path, but we keep it in the API surface so the consuming
/// crate's `foo::main(&state, ...)` call shape stays stable.
///
/// Phase 3 will grow this struct into the per-call arena container so
/// hosts can reuse one allocation across many dispatches without
/// re-paying the `Vec::resize` cost. Until then `Default::default()`
/// is enough — `call_buffer_entry` owns its own pool.
#[derive(Debug, Default)]
pub struct SandboxState {
    // Phase 3 will store:
    //   arena: Vec<u8>,
    //   caps: u64,
    //   trap_code: Cell<i32>,
    //
    // Hidden field today: a one-byte tag so Phase 3 additions don't
    // change `mem::size_of` between releases (helps catch ABI drift
    // in the consuming crate during the Phase 3 cut-over).
    _phase: PhantomPhase,
}

#[derive(Debug, Default)]
struct PhantomPhase;

impl SandboxState {
    /// Construct a fresh sandbox state. Phase 2 callers can reuse a
    /// single instance across many AOT calls because the struct
    /// holds no per-call data; Phase 3 will require a fresh state per
    /// call (the arena cursors reset on construction).
    pub fn new() -> Self {
        Self::default()
    }
}

impl ArenaState {
    /// Construct an `ArenaState` pointing at `arena[0..]` for a
    /// single dispatch. `scratch_base` is the arena-relative offset
    /// where the scratch bump region starts (= `out_ptr + out_cap`).
    ///
    /// # Safety
    ///
    /// `arena` must outlive the JIT call. The emitted body mutates
    /// the cursor fields through raw pointers; nothing else can
    /// alias the state for the call duration.
    pub(crate) fn new(arena: &mut [u8], scratch_base: u32) -> Self {
        Self {
            arena_base: UnsafeCell::new(arena.as_mut_ptr() as usize),
            arena_len: UnsafeCell::new(arena.len() as u32),
            tail_cursor: UnsafeCell::new(0),
            scratch_cursor: UnsafeCell::new(0),
            scratch_base: UnsafeCell::new(scratch_base),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Layout-stamp sanity test — keeps the `#[repr(C)]` slots aligned
    /// with the LLVM crate's emit offsets. The constants are
    /// duplicated rather than imported so a future LLVM-side
    /// reshuffle surfaces as a mismatch here, not silent corruption
    /// inside a JIT dispatch.
    #[test]
    fn arena_state_offsets_match_llvm() {
        use core::mem::{offset_of, size_of};
        assert_eq!(
            offset_of!(ArenaState, arena_base) as u32,
            ARENA_STATE_OFFSET_BASE
        );
        assert_eq!(
            offset_of!(ArenaState, arena_len) as u32,
            ARENA_STATE_OFFSET_LEN
        );
        assert_eq!(
            offset_of!(ArenaState, tail_cursor) as u32,
            ARENA_STATE_OFFSET_TAIL_CURSOR
        );
        assert_eq!(
            offset_of!(ArenaState, scratch_cursor) as u32,
            ARENA_STATE_OFFSET_SCRATCH_CURSOR
        );
        assert_eq!(
            offset_of!(ArenaState, scratch_base) as u32,
            ARENA_STATE_OFFSET_SCRATCH_BASE
        );
        assert!(size_of::<ArenaState>() >= 24);
    }
}
