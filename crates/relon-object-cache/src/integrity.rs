//! Integrity-mode selector for [`crate::storage::load`].
//!
//! Two modes exist:
//!
//! - [`IntegrityMode::Strict`] (default) — recompute SHA-256 over
//!   the object bytes on every load and compare against the value
//!   embedded in the filename. Costs ~1 us per MB on modern x86_64
//!   and protects against bit-rot or in-place tampering.
//!
//! - [`IntegrityMode::TrustOnWrite`] — skip the recompute. The
//!   writer guaranteed the filename matched the body's hash; the
//!   loader trusts that POSIX `mtime` + `size` have not changed
//!   underneath it. Strictly faster but offers no tamper detection
//!   on its own (still pairs sanely with HMAC).
//!
//! Production callers should leave the default. Benchmarks and very
//! large objects (> 1 MB) are the main reason the alternative
//! exists at all.

/// Selects the integrity policy used by [`crate::storage::load`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IntegrityMode {
    /// Recompute SHA-256 on each load. Default.
    #[default]
    Strict,
    /// Skip the recompute; trust the writer.
    TrustOnWrite,
}
