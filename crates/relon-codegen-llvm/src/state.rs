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
//! We **do not** reuse `relon_codegen_native::SandboxState` here on
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
}

/// Byte offset of [`ArenaState::arena_base`] inside the `#[repr(C)]`
/// layout. Used by the LLVM emitter to materialise the load.
pub const ARENA_STATE_OFFSET_BASE: u32 = 0;

/// Byte offset of [`ArenaState::arena_len`]. Reserved for Phase C
/// bounds-check work; the emitter leaves it untouched today.
#[allow(dead_code)]
pub const ARENA_STATE_OFFSET_LEN: u32 = std::mem::size_of::<usize>() as u32;

impl ArenaState {
    /// Construct a state that points at `arena[0..]` for a single
    /// dispatch. The caller owns the backing storage; this struct
    /// only borrows it through a raw pointer for the JIT's
    /// lifetime.
    ///
    /// # Safety
    ///
    /// The caller must keep `arena` live and exclusively owned by the
    /// `run_main` invocation that consumes this state. The emitted
    /// JIT code reads and writes through `arena_base` without
    /// touching the Rust borrow checker.
    pub fn new(arena: &mut [u8]) -> Self {
        Self {
            arena_base: UnsafeCell::new(arena.as_mut_ptr() as usize),
            arena_len: UnsafeCell::new(arena.len() as u32),
        }
    }
}
