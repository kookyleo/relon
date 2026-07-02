//! Integrity-mode selector for [`crate::storage::load`].
//!
//! Two modes exist:
//!
//! - [`IntegrityMode::Strict`] (default) — recompute the
//!   content-addressing key on every load and compare against the
//!   value embedded in the filename. The key
//!   ([`crate::storage::content_key`]) is a SHA-256 over the object
//!   bytes **and** the security-relevant metadata (`cap_bitmap`,
//!   `host_fn_imports`, `main_signature`), so in-place tampering of
//!   either the object body or the metadata trailer — e.g. flipping
//!   `cap_bitmap` to "all caps on" — is caught even with no HMAC key.
//!   Costs ~1 us per MB on modern x86_64 and also guards against
//!   bit-rot. Only valid when the caller derives the filename stem
//!   from [`crate::storage::content_key`].
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
//! Production callers should use `Strict` when they hash the object
//! into the filename, or `HmacRequired` when they hash something
//! else into the filename and rely on HMAC for integrity.
//!
//! ## Removed: `TrustOnWrite`
//!
//! Earlier revisions exposed a third variant, `TrustOnWrite`, that
//! skipped both the SHA-256 recompute *and* the HMAC enforcement.
//! It was a footgun: any caller who hashed a source-derived key
//! into the filename stem (rather than the object body's own
//! digest) and forgot to pass an HMAC key would silently downgrade
//! to a no-integrity load, the exact bypass #171 closed at the
//! integration layer. The variant has been removed in v0.x — use
//! `HmacRequired` (with a real HMAC key) for the same use case
//! while keeping tamper detection.

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
