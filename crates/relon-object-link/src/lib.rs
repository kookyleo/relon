//! `relon-object-link` — the missing link pass between
//! `relon-codegen-cranelift` and `relon-object-cache` in the v5-gamma
//! native AOT pipeline.
//!
//! ## Why this crate exists
//!
//! `cranelift-object` emits relocatable ELF objects (`ET_REL`, the
//! flavour you would normally hand to `ar`). The Linux dynamic loader
//! (`glibc` / `musl`) strictly requires `ET_DYN` for `dlopen`, which
//! means the `.o` cranelift produces cannot be loaded directly even
//! via the `memfd_create` + `/proc/self/fd/<n>` trick implemented in
//! `relon-object-cache`. Some external link step has to turn that
//! `.o` into a position-independent shared object first.
//!
//! Concretely, the v5-gamma cold-start pipeline becomes:
//!
//! ```text
//! source -> cranelift IR -> ObjectModule::finish().emit() (ET_REL)
//!                              |
//!                              v
//!                  relon_object_link::link_to_dyn  (this crate)
//!                              |
//!                              v
//!                  relon_object_cache::store / load
//!                              |
//!                              v
//!                  LoadedObject::from_bytes (dlopen ET_DYN)
//! ```
//!
//! ## Modules
//!
//! - [`elf_check`] — hand-rolled 64-bit ELF header parser so we do
//!   not pull `object` / `goblin` for ~20 bytes of header.
//! - [`linker_subproc`] — default linker. Shells out to system `ld`
//!   (or `cc -shared`) via `Command`, captures stderr on failure.
//! - `linker_lld` — feature-gated (`lld-inproc`) in-process `lld`
//!   linker stub. Currently returns [`LinkError::FeatureNotImplemented`]
//!   because the `lld-sys` crate is not yet on a stable release.
//! - [`error`] — the public [`LinkError`] enum.
//!
//! ## Platform support
//!
//! Linux x86_64 is the only tier-1 platform for v5-gamma. The
//! subprocess linker is gated on `cfg(unix)`; macOS / Windows will
//! need their own backends (`ld64`, `link.exe`) and surface
//! [`LinkError::UnsupportedTriple`] from `link_to_dyn` for now.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod elf_check;
pub mod error;
#[cfg(unix)]
pub mod linker_subproc;

#[cfg(feature = "lld-inproc")]
pub mod linker_lld;

pub use elf_check::{is_et_dyn, is_et_rel, parse_elf_type, ElfType};
pub use error::LinkError;
#[cfg(unix)]
pub use linker_subproc::SubprocLinker;

#[cfg(feature = "lld-inproc")]
pub use linker_lld::LldLinker;

/// Top-level entry point: link an `ET_REL` relocatable object into an
/// `ET_DYN` shared object using the default (subprocess) backend.
///
/// `target_triple` follows the cranelift / `target-lexicon` form
/// (`x86_64-unknown-linux-gnu`, …). Only `x86_64-*-linux-*` triples
/// are accepted today; everything else returns
/// [`LinkError::UnsupportedTriple`].
#[cfg(unix)]
pub fn link_to_dyn(et_rel_bytes: &[u8], target_triple: &str) -> Result<Vec<u8>, LinkError> {
    let linker = SubprocLinker::new()?;
    linker.link(et_rel_bytes, target_triple)
}

#[cfg(not(unix))]
pub fn link_to_dyn(_et_rel_bytes: &[u8], target_triple: &str) -> Result<Vec<u8>, LinkError> {
    Err(LinkError::UnsupportedTriple(target_triple.to_owned()))
}
