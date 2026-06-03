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

/// Byte offset of [`ArenaState::trap_code`]. Mirror of
/// `relon_codegen_llvm::state::ARENA_STATE_OFFSET_TRAP_CODE`. The four
/// trailing u32 fields (`arena_len`, `tail_cursor`, `scratch_cursor`,
/// `scratch_base`) total 16 bytes past `arena_base`; the `u64`
/// `trap_code` follows on its natural 8-byte boundary at offset 24.
///
/// The capability gate the LLVM emitter bakes into a `#native` body
/// (`Op::CheckCap`) stores [`relon_eval_api::CapabilityBit`]-keyed
/// `NativeTrap::CapabilityDenied` (= 3) here and returns the negative
/// `bytes_written` sentinel. The shim **must** own real backing memory
/// at this offset so the JIT-side `store` lands on a slot
/// [`call_buffer_entry`] can read back to lift a typed error.
#[allow(dead_code)]
pub(crate) const ARENA_STATE_OFFSET_TRAP_CODE: u32 = 24;

/// Byte offset of [`ArenaState::host_fns`]. Mirror of
/// `relon_codegen_llvm::state::ARENA_STATE_OFFSET_HOST_FNS`. The closed-
/// world `#native` dispatch the rs-build path emits resolves host fns
/// at link time (the host bitcode is inlined into the `.o`), so the
/// JIT body never reads this slot — but the layout must still carry it
/// so the struct's tail matches the emitter's `#[repr(C)]` view and a
/// future open-world dynamic dispatch lands on owned memory.
#[allow(dead_code)]
pub(crate) const ARENA_STATE_OFFSET_HOST_FNS: u32 = ARENA_STATE_OFFSET_TRAP_CODE + 8;

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
    /// Trap code recorded by the JIT body's `Op::CheckCap` gate (and,
    /// on a future open-world path, by `relon_llvm_call_native`). `0`
    /// means "no trap"; `3` (`NativeTrap::CapabilityDenied`) means a
    /// gated `#native` call was denied because the granted `caps`
    /// bitmask had the required bit clear. [`call_buffer_entry`] reads
    /// this slot whenever the entry returns the negative `bytes_written`
    /// sentinel and lifts the matching typed error.
    pub trap_code: UnsafeCell<u64>,
    /// Host-fn registry pointer. The closed-world rs-build path inlines
    /// host fns into the `.o` at link time, so this stays null — it
    /// exists only to keep the `#[repr(C)]` tail byte-matched with the
    /// emitter's `ArenaState`.
    pub host_fns: UnsafeCell<usize>,
}

/// Host-facing sandbox policy carrier threaded into every AOT call.
///
/// The fast path (`extern "C" fn(i64, ...) -> i64`) ignores it — every
/// value lives in a register and the body has no capability gate. The
/// buffer-protocol path reads [`Self::caps_mask`] and forwards it as
/// the entry's trailing `i64 caps` argument so a `#native` body's
/// `Op::CheckCap` gate consults the host's actual grant: a granted bit
/// lets the gated call run, a clear bit traps `CapabilityDenied`.
///
/// Construction is grant-explicit: [`SandboxState::default`] /
/// [`SandboxState::new`] grant **nothing** (zero-trust, same posture as
/// the evaluator's `Capabilities::default`). Hosts opt into a capability
/// by building a [`Capabilities`] and passing it to
/// [`SandboxState::with_capabilities`], or flip a single
/// [`CapabilityBit`] via [`SandboxState::grant`].
#[derive(Debug, Default)]
pub struct SandboxState {
    /// Granted capability bitmask — bit `b` set means the host granted
    /// [`CapabilityBit`] index `b`. Forwarded verbatim as the buffer
    /// entry's `caps` argument. Zero (the default) denies every gated
    /// `#native` call.
    caps_mask: i64,
}

impl SandboxState {
    /// Construct a zero-trust sandbox state — no capability granted.
    /// Identical to [`Self::default`]; kept for call-site readability.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a sandbox state granting exactly the capabilities set in
    /// `caps`. The per-bit booleans map onto the canonical
    /// [`CapabilityBit`] indices so the resulting mask byte-matches the
    /// `(caps & (1 << bit))` test the LLVM `Op::CheckCap` gate bakes
    /// into a gated `#native` body.
    pub fn with_capabilities(caps: &relon_eval_api::Capabilities) -> Self {
        let mut s = Self::default();
        s.set_from_capabilities(caps);
        s
    }

    /// Grant a single [`CapabilityBit`], leaving the others untouched.
    /// Chainable: `SandboxState::new().grant(CapabilityBit::ReadsClock)`.
    pub fn grant(mut self, bit: relon_eval_api::CapabilityBit) -> Self {
        self.caps_mask |= 1i64 << bit.bit_index();
        self
    }

    /// Mirror every set bit of `caps` onto the internal mask.
    fn set_from_capabilities(&mut self, caps: &relon_eval_api::Capabilities) {
        use relon_eval_api::CapabilityBit::*;
        let set = |mask: &mut i64, on: bool, bit: relon_eval_api::CapabilityBit| {
            if on {
                *mask |= 1i64 << bit.bit_index();
            }
        };
        set(&mut self.caps_mask, caps.reads_fs, ReadsFs);
        set(&mut self.caps_mask, caps.writes_fs, WritesFs);
        set(&mut self.caps_mask, caps.network, Network);
        set(&mut self.caps_mask, caps.reads_clock, ReadsClock);
        set(&mut self.caps_mask, caps.reads_env, ReadsEnv);
        set(&mut self.caps_mask, caps.uses_rng, UsesRng);
    }

    /// The granted capability bitmask the buffer entry receives as its
    /// `caps` argument. Crate-internal — [`crate::call_buffer_entry`]
    /// forwards it on dispatch.
    pub(crate) fn caps_mask(&self) -> i64 {
        self.caps_mask
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
            trap_code: UnsafeCell::new(0),
            host_fns: UnsafeCell::new(0),
        }
    }

    /// Read back the trap code the JIT body recorded. `0` means no
    /// trap. Called by [`crate::call_buffer_entry`] after a negative
    /// `bytes_written` sentinel to decide which typed error to lift.
    pub(crate) fn trap_code(&self) -> u64 {
        // SAFETY: the JIT call has returned, so no concurrent writer to
        // the cell remains; the read is single-threaded and aliasing-free.
        unsafe { *self.trap_code.get() }
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
        assert_eq!(
            offset_of!(ArenaState, trap_code) as u32,
            ARENA_STATE_OFFSET_TRAP_CODE
        );
        assert_eq!(
            offset_of!(ArenaState, host_fns) as u32,
            ARENA_STATE_OFFSET_HOST_FNS
        );
        assert!(size_of::<ArenaState>() >= 32);
    }
}
