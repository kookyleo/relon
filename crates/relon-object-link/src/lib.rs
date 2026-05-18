//! `relon-object-link` — the missing link pass between
//! `relon-codegen-native` and `relon-object-cache` in the v5-gamma
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
//! - [`error`] — the public [`LinkError`] enum.
//!
//! Subsequent commits in this series wire in the subprocess linker
//! (`linker_subproc`) and the feature-gated in-process lld stub
//! (`linker_lld`), along with the top-level [`link_to_dyn`] entry
//! point.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod elf_check;
pub mod error;

pub use elf_check::{is_et_dyn, is_et_rel, parse_elf_type, ElfType};
pub use error::LinkError;
