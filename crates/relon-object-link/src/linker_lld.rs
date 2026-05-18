//! In-process `lld` linker — feature-gated stub.
//!
//! ## Status
//!
//! The `lld-sys` / `lld` crates are not on a stable release channel
//! as of v5-gamma planning, so this module deliberately does **not**
//! pull them in. The public shape ([`LldLinker`] + `link`) is
//! defined so the codegen-native crate can write call sites against
//! a stable API, but every entry point returns
//! [`LinkError::FeatureNotImplemented`].
//!
//! A future v5-gamma phase decision will replace the body with one
//! of:
//!
//! - `lld-sys` direct bindings — fastest, but currently pre-1.0.
//! - `lld` crate (which wraps `LLDMain`) — easier API, larger build.
//! - Vendored mold / wild — same trade-off as lld.
//!
//! Until that decision lands, downstream code should use
//! [`crate::SubprocLinker`] (the default). This stub exists so a
//! `--features lld-inproc` build still compiles, lets us swap the
//! implementation without an API break.

use crate::error::LinkError;

/// In-process linker handle. Currently a zero-sized stub; once a
/// real backend lands it will own the per-process lld context so
/// repeated [`LldLinker::link`] calls do not re-initialise the
/// linker tables.
#[derive(Debug, Default, Clone, Copy)]
pub struct LldLinker;

impl LldLinker {
    /// Construct the stub linker. Returns
    /// [`LinkError::FeatureNotImplemented`] eagerly so callers can
    /// fall back to the subprocess backend at startup rather than at
    /// link time.
    pub fn new() -> Result<Self, LinkError> {
        Err(LinkError::FeatureNotImplemented)
    }

    /// Link `et_rel_bytes` into an `ET_DYN` shared object. Stub —
    /// always returns [`LinkError::FeatureNotImplemented`].
    pub fn link(&self, _et_rel_bytes: &[u8], _target_triple: &str) -> Result<Vec<u8>, LinkError> {
        Err(LinkError::FeatureNotImplemented)
    }
}
