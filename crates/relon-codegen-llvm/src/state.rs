//! Minimal runtime state for the LLVM AOT backend's buffer-protocol
//! entries. **Phase B.**
//!
//! The buffer-protocol entry signature mirrors the cranelift-native
//! backend's `EntryShape::BufferProtocol`:
//!
//! ```text
//! fn run_main(state: *const SandboxState,
//!             in_ptr: i32, in_len: i32,
//!             out_ptr: i32, out_cap: i32,
//!             caps: i64) -> i32;
//! ```
//!
//! `LoadField` / `StoreField` ops resolve to absolute host addresses
//! through the formula `arena_base + buf_ptr + offset`, where
//! `arena_base` lives at a stable offset on the state. The LLVM
//! emitter loads it through a `ptrtoint`/`inttoptr` round-trip.
//!
//! We **do not** reuse `relon_codegen_cranelift::SandboxState` here on
//! purpose:
//!
//! - It would require pulling cranelift-native as a hard dependency of
//!   the LLVM crate just to share an opaque struct layout. The LLVM
//!   backend is meant to stand on its own.
//! - The Phase B envelope does not need sandbox traps, deadline
//!   checks, capability bits, or the closure / scratch / trap_code
//!   subsystems. A minimal C-layout `ArenaState` is enough to drive
//!   the buffer protocol.
//! - Keeping the layout local to this crate makes the offsets we
//!   embed in emitted LLVM IR self-contained — if the cranelift
//!   crate ever rearranges `SandboxState` it cannot accidentally
//!   miscompile our IR.
//!
//! Phase C (when sandbox traps + closures land) is the right time to
//! revisit the dep direction; for Phase B this stays self-contained.

use std::cell::UnsafeCell;

/// Per-call arena state handed to the LLVM JIT-compiled entry. The
/// emitter reads `arena_base` (at offset 0 on a 64-bit host) and
/// `arena_len` (offset 8) to resolve every buffer-protocol load /
/// store; everything past those two fields is reserved for Phase C
/// (sandbox traps, deadline, closure table).
///
/// `#[repr(C)]` because the LLVM emitter hard-codes the field
/// offsets through `inttoptr(arena_base_ptr + N)` style address
/// arithmetic.
///
/// `UnsafeCell` on the live fields because the JIT thread mutates
/// them through a raw pointer; Rust's borrow checker cannot see the
/// emitted machine code. The per-call ownership model (one
/// `ArenaState` per `run_main` dispatch) means no aliasing race
/// can occur — the LLVM evaluator allocates a fresh state on the
/// stack before each call.
#[repr(C)]
pub struct ArenaState {
    /// Base pointer of the arena bytes the host owns. The emitted
    /// LLVM IR reads this through `load i64, ptr %state` (offset 0),
    /// then `inttoptr` to a byte pointer + i64-extended `buf_ptr` +
    /// `field_offset`. The pointer is `usize`-wide so the cast
    /// matches the host's pointer width.
    pub arena_base: UnsafeCell<usize>,
    /// Length of the arena in bytes. Phase B does not emit bounds
    /// checks (the task spec explicitly notes div0 / overflow /
    /// bounds are exposed as `llvm.trap` / panic at most), so this
    /// is recorded for future use rather than read by the JIT today.
    pub arena_len: UnsafeCell<u32>,
    /// Phase E.1: tail cursor used by pointer-indirect StoreField
    /// (`String` / `ListInt` / `ListFloat` / `ListBool`) to bump-
    /// allocate records inside the output buffer's tail region.
    /// Counts buffer-relative bytes from `out_ptr`. Reset to 0 at the
    /// start of every dispatch.
    pub tail_cursor: UnsafeCell<u32>,
    /// Phase E.1: scratch bump cursor used by stdlib bodies (`concat`,
    /// `substring`, ...) and `Op::StrConcatN` to allocate temporary
    /// records inside the arena's scratch region. Counts bytes from
    /// `scratch_base`. Reset to 0 per dispatch.
    pub scratch_cursor: UnsafeCell<u32>,
    /// Phase E.1: arena-relative byte offset at which the scratch
    /// region starts (= `out_ptr + out_cap`). The bump path reads
    /// `scratch_base + scratch_cursor` as the i32 pointer returned to
    /// the stdlib body.
    pub scratch_base: UnsafeCell<u32>,
}

/// Byte offset of [`ArenaState::arena_base`] inside the `#[repr(C)]`
/// layout. Used by the LLVM emitter to materialise the load.
pub const ARENA_STATE_OFFSET_BASE: u32 = 0;

/// Byte offset of [`ArenaState::arena_len`]. Reserved for Phase C
/// bounds-check work; the emitter leaves it untouched today.
#[allow(dead_code)]
pub const ARENA_STATE_OFFSET_LEN: u32 = std::mem::size_of::<usize>() as u32;

/// Byte offset of [`ArenaState::tail_cursor`]. The pointer-indirect
/// StoreField path loads and stores this u32 to bump-allocate the
/// output buffer's tail region.
pub const ARENA_STATE_OFFSET_TAIL_CURSOR: u32 = ARENA_STATE_OFFSET_LEN + 4;

/// Byte offset of [`ArenaState::scratch_cursor`]. Loaded / stored by
/// the `Op::AllocScratch` / `Op::AllocScratchDyn` lowering.
pub const ARENA_STATE_OFFSET_SCRATCH_CURSOR: u32 = ARENA_STATE_OFFSET_TAIL_CURSOR + 4;

/// Byte offset of [`ArenaState::scratch_base`]. Loaded by the scratch
/// allocator to compute the arena-relative offset of a freshly-
/// reserved scratch block (`scratch_base + scratch_cursor`).
pub const ARENA_STATE_OFFSET_SCRATCH_BASE: u32 = ARENA_STATE_OFFSET_SCRATCH_CURSOR + 4;

impl ArenaState {
    /// Construct a state that points at `arena[0..]` for a single
    /// dispatch. The caller owns the backing storage; this struct
    /// only borrows it through a raw pointer for the JIT's
    /// lifetime.
    ///
    /// `scratch_base` is the arena-relative offset where temporary
    /// allocations (string concat, ...) live; pass `arena.len()` to
    /// disable the scratch path. The cursors are reset to 0 so the
    /// JIT bump path starts fresh on every dispatch.
    ///
    /// # Safety
    ///
    /// The caller must keep `arena` live and exclusively owned by the
    /// `run_main` invocation that consumes this state. The emitted
    /// JIT code reads and writes through `arena_base` without
    /// touching the Rust borrow checker.
    pub fn new(arena: &mut [u8], scratch_base: u32) -> Self {
        Self {
            arena_base: UnsafeCell::new(arena.as_mut_ptr() as usize),
            arena_len: UnsafeCell::new(arena.len() as u32),
            tail_cursor: UnsafeCell::new(0),
            scratch_cursor: UnsafeCell::new(0),
            scratch_base: UnsafeCell::new(scratch_base),
        }
    }

    /// Read the current tail-cursor value. Used by the evaluator
    /// after a dispatch returns to know how much was written into the
    /// tail region (for `String` return-value decoding).
    #[allow(dead_code)]
    pub fn tail_cursor(&self) -> u32 {
        // SAFETY: caller owns the state exclusively for a single
        // dispatch — no aliasing read can happen.
        unsafe { *self.tail_cursor.get() }
    }
}
