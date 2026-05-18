//! Public error surface for the link pass.
//!
//! Every error variant carries enough context for an operator to
//! debug a failed cold-start without re-running the codegen agent in
//! a debugger: paths, full linker stderr, the ELF type we actually
//! saw, etc.

use crate::elf_check::ElfType;

/// Anything that can go wrong while turning an `ET_REL` object into
/// a loadable `ET_DYN` shared object.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// Input bytes do not parse as a 64-bit little-endian ELF header.
    /// We bail before invoking the linker so the caller gets a clear
    /// "this is not an ELF object" message instead of a cryptic
    /// `ld: file format not recognized` stderr blob.
    #[error("invalid elf input: {0}")]
    InvalidElf(String),

    /// Input parsed as ELF but was not `ET_REL`. Most likely the
    /// caller already linked the object, or fed us an executable.
    #[error("input is not ET_REL: got {0:?}")]
    NotEtRel(ElfType),

    /// The linker returned successfully but produced something other
    /// than `ET_DYN`. Indicates a missing `-shared` flag on a custom
    /// linker path, or a linker that silently downgraded the output.
    #[error("output is not ET_DYN: got {0:?}")]
    NotEtDyn(ElfType),

    /// No usable linker (`ld`, `cc`, or the `RELON_LD` override)
    /// found on `$PATH`. Cold-start cannot proceed; the host should
    /// fall back to the cranelift-jit warm path.
    #[error("ld binary not found in PATH")]
    LinkerNotFound,

    /// Linker exited non-zero. Carries the captured stderr so a CI
    /// log shows what the linker actually complained about.
    #[error("linker invocation failed: {0}")]
    LinkerFailed(String),

    /// Generic I/O — typically writing the tempfile we hand to `ld`,
    /// or reading the linker's output back.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Triple is well-formed but we do not yet ship a linker config
    /// for it. v5-gamma is x86_64-linux only.
    #[error("unsupported target triple: {0}")]
    UnsupportedTriple(String),

    /// The `lld-inproc` feature is compiled in but the stub has not
    /// been replaced with a real `lld-sys` binding yet. The
    /// subprocess linker remains the supported path.
    #[error("in-process lld feature not implemented")]
    FeatureNotImplemented,
}
