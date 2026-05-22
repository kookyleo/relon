//! `relon-object-cache` — on-disk + in-memory cache layer for the
//! cranelift-object `.o` artefacts produced by `relon-codegen-native`
//! during the v5-gamma cold-start pipeline.
//!
//! The crate is intentionally self-contained: it does **not** depend
//! on the rest of the `relon-*` tree, so the codegen-native agent
//! can wire it in incrementally without circular-build problems.
//!
//! ## What lives here
//!
//! - [`storage`] — `<sha256>.relon-native-v1` file layout, atomic
//!   write, integrity-checked read.
//! - [`hmac`] — per-installation HMAC-SHA256 key, stored mode-`0600`
//!   under `$XDG_DATA_HOME/relon/cache-key`, used to authenticate
//!   cache files against third-party injection.
//! - [`integrity`] — `Strict` (re-hash on every load),
//!   `HmacRequired` (rely on the HMAC trailer for integrity; refuses
//!   keyless loads), and the legacy `TrustOnWrite` variant kept only
//!   for backward compatibility with older tests. New production
//!   callers should use `HmacRequired` whenever the filename stem
//!   does not equal the object body's own SHA-256.
//! - [`loader`] — Linux `memfd_create` + `/proc/self/fd/<n>` dlopen +
//!   dlsym path that turns cached bytes back into callable function
//!   pointers without touching disk on the warm path. macOS / Windows
//!   error with [`LoaderError::UnsupportedPlatform`] for now (gamma
//!   phase is Linux-only).
//! - [`error`] — the public [`CacheError`] / [`LoaderError`] /
//!   [`HmacError`] enums.
//!
//! See `docs/internal/v5-gamma-cranelift-object-cache-design.md` for
//! the full file-format and threat-model rationale.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod error;
pub mod hmac;
pub mod integrity;
pub mod loader;
pub mod storage;

pub use error::{CacheError, HmacError, LoaderError};
pub use hmac::{compute_hmac, ensure_key, hmac_key_path, verify_hmac};
pub use integrity::IntegrityMode;
pub use loader::{LoadedObject, ObjectHandle};
pub use storage::{load, store, CacheEntry, HostFnImport, Metadata, SignatureHash};
