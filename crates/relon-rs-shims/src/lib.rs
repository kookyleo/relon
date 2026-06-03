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
//! - Capability threading on the buffer path — `call_buffer_entry`
//!   hard-codes `caps = 0`, so `#native` / `Op::CheckCap`-gated bodies
//!   aren't yet reachable through this crate
//! - Structured trap propagation (today a Relon-side trap surfaces as
//!   a Rust panic; Phase 3 will catch + return a typed `Result`)
//! - Cross-platform host shim coverage (macOS / Windows AOT linking)

#![warn(missing_docs)]

pub mod marshal;
pub mod sandbox_state;
pub mod shims;

pub use marshal::{
    call_buffer_entry, ArgValue, BufferEntryError, BufferEntryFn, EmittedField, EmittedFieldType,
    RetValue,
};
pub use sandbox_state::SandboxState;

// `relon_llvm_str_contains_arena` is the only host shim symbol the
// LLVM AOT pipeline references today (Phase F.1 `str.contains` fast
// path). The function lives in the `shims` module under `#[no_mangle]`
// — the static-link consumer picks it up via plain ELF symbol
// resolution when the build.rs side advertises this crate as a
// `staticlib` linker dep.
#[doc(hidden)]
pub use shims::relon_llvm_str_contains_arena;
