//! Integrity-mode selector for [`crate::storage::load`].
//!
//! Three modes exist:
//!
//! - [`IntegrityMode::Strict`] (default) — recompute SHA-256 over
//!   the object bytes on every load and compare against the value
//!   embedded in the filename. Costs ~1 us per MB on modern x86_64
//!   and protects against bit-rot or in-place tampering. Only
//!   valid when the caller derives the filename stem from the
//!   object body's own SHA-256.
//!
//! - [`IntegrityMode::HmacRequired`] — skip the SHA-256 recompute
//!   because the caller routes a different value through the
//!   filename stem (typically a source / metadata digest, not the
//!   object body's hash). The trailing HMAC tag is **mandatory** in
//!   this mode: the loader refuses any file with `hmac_key = None`
//!   and the HMAC verification covers the entire body (header +
//!   object bytes + metadata), so in-place tampering is still
//!   caught — just by the HMAC layer instead of by SHA-256.
//!
//! - [`IntegrityMode::TrustOnWrite`] — deprecated alias retained
//!   for tests; behaves identically to `HmacRequired` minus the
//!   "HMAC mandatory" enforcement. New production callers must
//!   prefer `HmacRequired`.
//!
//! Production callers should use `Strict` when they hash the object
//! into the filename, or `HmacRequired` when they hash something
//! else into the filename and rely on HMAC for integrity.

use crate::error::CacheError;

/// Selects the integrity policy used by [`crate::storage::load`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IntegrityMode {
    /// Recompute SHA-256 on each load. Default.
    #[default]
    Strict,
    /// Skip SHA-256 recompute but mandate that the trailing HMAC
    /// tag is verified — the load fails with
    /// [`CacheError::HmacKeyRequired`] when the caller passes
    /// `hmac_key = None`. This is the production mode for callers
    /// who hash a source-derived key (not the object body) into the
    /// filename stem.
    HmacRequired,
    /// Skip the recompute; trust the writer. Retained for legacy
    /// tests only — new code must use [`IntegrityMode::HmacRequired`]
    /// so the loader cannot silently fall back to no-integrity mode.
    #[deprecated(
        since = "0.1.0",
        note = "use Strict when the filename is the object hash, or HmacRequired when integrity is provided by the HMAC tag"
    )]
    TrustOnWrite,
}

impl IntegrityMode {
    /// Returns `Err(CacheError::HmacKeyRequired)` when the mode
    /// demands a present HMAC key but the caller did not supply one.
    pub(crate) fn enforce_hmac_present(
        self,
        hmac_key: Option<&[u8; 32]>,
    ) -> Result<(), CacheError> {
        if matches!(self, IntegrityMode::HmacRequired) && hmac_key.is_none() {
            return Err(CacheError::HmacKeyRequired);
        }
        Ok(())
    }
}
