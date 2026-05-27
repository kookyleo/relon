//! Runtime ABI shims for Relon AOT-compiled functions linked into Rust
//! binaries.
//!
//! ## What this crate is
//!
//! `relon-rs-build` (the build.rs side) compiles `.relon` sources to
//! relocatable ELF object files at build time, and `relon-rs-macro`
//! (`include_relon!`) splices the matching `extern "C"` declarations
//! into the consuming Rust source. The two halves meet at a stable
//! ABI surface that this crate owns.
//!
//! ## Phase 1 envelope
//!
//! Phase 1 supports `#main(Int...) -> Int` only. Under that envelope
//! every parameter is passed by value as `i64` and the return is a
//! single `i64` — no `SandboxState` pointer is needed because the
//! body is a leaf arithmetic function with no arena dependency. The
//! consuming Rust code looks like:
//!
//! ```ignore
//! relon_rs_macro::include_relon!("src/foo.relon");
//!
//! fn main() {
//!     let state = relon_rs_shims::SandboxState::default();
//!     let result = foo::main(&state, 42);
//!     println!("{}", result);
//! }
//! ```
//!
//! The `&SandboxState` argument is unused under Phase 1 (the AOT body
//! ignores it), but the shim threads it through verbatim so the
//! `foo::main` Rust signature stays stable across the Phase 2 surface
//! widening — Phase 2 wires through arena allocation, scratch
//! cursors, and host-shim symbols (`__relon_str_contains_arena`, …)
//! at which point `SandboxState` carries the per-call arena pointer.
//!
//! ## Why a placeholder today
//!
//! Keeping `SandboxState` in the public surface from Phase 1 lets
//! downstream consumers (the demo crate + future call sites) write
//! `foo::main(&state, ...)` once and not rewrite when Phase 2 lands.
//! The body of the struct is empty for Phase 1; Phase 2 grows it
//! into:
//! - `arena_base: *mut u8`
//! - `arena_len: u32`
//! - `scratch_cursor: u32`
//! - `tail_cursor: u32`
//! - capability bits, deadline slot, trap-code slot
//!
//! matching the LLVM crate's internal `ArenaState` layout once the
//! Phase 2 ABI freezes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Forward-compat sandbox state placeholder.
///
/// Phase 1 keeps this empty because the Int-only `#main(Int...) -> Int`
/// envelope has no arena dependency — every value lives in a register.
/// Phase 2 grows the struct to carry the per-call arena base pointer,
/// scratch cursors, and trap slot so pointer-indirect args (String /
/// List) and host-shim calls can land without changing the consuming-
/// crate's `foo::main(&state, ...)` call shape.
///
/// `Default::default()` is the only constructor needed today; Phase 2
/// will add `with_arena_cap(usize)` / `with_caps(u64)` builders for
/// configuring the runtime state per call.
#[derive(Debug, Default)]
pub struct SandboxState {
    // Phase 2 will store:
    //   arena: Vec<u8>,
    //   scratch_cursor: Cell<u32>,
    //   tail_cursor: Cell<u32>,
    //   caps: u64,
    //   trap_code: Cell<i32>,
    //
    // Hidden field today: a one-byte tag so Phase 2 additions don't
    // change `mem::size_of` between releases (helps catch ABI drift
    // in the consuming crate during the Phase 2 cut-over).
    _phase: PhantomPhase,
}

#[derive(Debug, Default)]
struct PhantomPhase;

impl SandboxState {
    /// Construct a fresh per-call sandbox state. Phase 1 callers can
    /// reuse a single instance across many AOT calls because the
    /// struct holds no per-call data; Phase 2 will require a fresh
    /// state per call (the arena cursors reset on construction).
    pub fn new() -> Self {
        Self::default()
    }
}
