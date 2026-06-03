//! Runtime ABI shims for Relon AOT-compiled functions linked into Rust
//! binaries.
//!
//! ## Crate role
//!
//! `relon-rs-build` (the build.rs side) compiles `.relon` sources to
//! relocatable ELF object files at build time, and `relon-rs-macro`
//! (`include_relon!`) splices the matching `extern "C"` declarations
//! into the consuming Rust source. Whatever the generated binding does
//! at runtime — pack typed Rust args into the arena, invoke the JIT
//! entry, decode the return record, expose host shim symbols the JIT
//! body calls back into — is implemented here.
//!
//! ## Entry shapes
//!
//! Two entry shapes flow through this crate:
//!
//! - **Fast-path (`FastInt`)** — `#main(Int...) -> Int` with arity <= 8.
//!   The binding declares an `extern "C" fn(i64, ...) -> i64` and
//!   invokes it directly; nothing in this crate is touched at runtime
//!   beyond the placeholder [`SandboxState`] threaded through the
//!   signature for forward compatibility. Matches the Phase 1 trivial
//!   demo path.
//! - **Buffer-protocol (`Buffer`)** — every other supported shape.
//!   The accepted leaf types are `Int`, `Float`, `Bool`, `Null`,
//!   `String`, and `List<Int>` (mirrored by the [`EmittedFieldType`]
//!   enum). The binding builds an [`ArgValue`] vector from typed Rust
//!   args, hands it to [`call_buffer_entry`], which packs the arena,
//!   dispatches the JIT, and decodes the return record into a
//!   [`RetValue`]. `f64` params/returns ride an 8-byte inline slot;
//!   `&str` / `&[i64]` are copied into the arena's pointer-indirect
//!   tail region.
//!
//! ## Host shim symbols
//!
//! `relon-codegen-llvm`'s emitter routes hot-path `str.contains` calls
//! through the [`relon_llvm_str_contains_arena`] extern. The shim is
//! defined here (rather than re-exported from `relon-codegen-llvm`) so
//! the user binary's link doesn't pull `llvm-sys` / `inkwell` into the
//! runtime — `relon-rs-shims` stays a thin static-link surface.
//!
//! ## What's deferred to Phase 3
//!
//! - `List<Float>` / `List<List<…>>` / nested-`Schema` argument /
//!   return marshalling (`Int` / `Float` / `Bool` / `Null` / `String` /
//!   `List<Int>` are wired today)
//! - Closure-valued returns
//! - Cross-platform host shim coverage (macOS / Windows AOT linking)
//!
//! ## Capability threading
//!
//! [`SandboxState`] carries the host's granted capability bitmask.
//! [`call_buffer_entry`] forwards it as the buffer entry's trailing
//! `i64 caps` argument, so a `#native` body's `Op::CheckCap` gate
//! consults the host's actual grant: a granted bit lets the gated call
//! run, a clear bit traps. A denied gate records
//! `NativeTrap::CapabilityDenied` in `ArenaState::trap_code` + returns
//! the negative sentinel, which the marshaller lifts to a typed
//! [`marshal::BufferEntryError::CapabilityDenied`] (no SIGILL / panic).
//! Hosts grant via [`SandboxState::with_capabilities`] /
//! [`SandboxState::grant`]; the default state is zero-trust.

#![warn(missing_docs)]

pub mod marshal;
pub mod sandbox_state;
pub mod shims;

pub use marshal::{
    call_buffer_entry, ArgValue, BufferEntryError, BufferEntryFn, EmittedField, EmittedFieldType,
    RetValue,
};
pub use sandbox_state::SandboxState;

// Capability surface the consuming crate uses to build a granted
// `SandboxState`. Re-exported from `relon-eval-api` (which re-exports
// the canonical `relon-cap` definitions) so a consumer can name
// `relon_rs_shims::CapabilityBit::ReadsClock` without taking a direct
// dep on the analyzer / cap crate.
pub use relon_eval_api::{Capabilities, CapabilityBit};

// `relon_llvm_str_contains_arena` is the only host shim symbol the
// LLVM AOT pipeline references today (Phase F.1 `str.contains` fast
// path). The function lives in the `shims` module under `#[no_mangle]`
// — the static-link consumer picks it up via plain ELF symbol
// resolution when the build.rs side advertises this crate as a
// `staticlib` linker dep.
#[doc(hidden)]
pub use shims::relon_llvm_str_contains_arena;
