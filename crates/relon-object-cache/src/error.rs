//! Error enums for the object-cache crate.
//!
//! Three independent surfaces:
//!
//! - [`CacheError`] — storage-side problems (I/O, malformed file,
//!   HMAC / SHA-256 mismatch, version skew).
//! - [`LoaderError`] — runtime loading problems (memfd, dlopen,
//!   dlsym, unsupported OS).
//! - [`HmacError`] — issues with the per-installation HMAC key file
//!   itself (missing parent dir, wrong mode bits, getrandom failure).
//!
//! They are split rather than merged because callers usually want to
//! react differently — a stale cache file should be silently
//! invalidated and regenerated; a dlopen failure is fatal.

use thiserror::Error;

/// Anything that can go wrong while reading or writing a cache
/// blob on disk.
#[derive(Debug, Error)]
pub enum CacheError {
    /// Underlying filesystem error. Bubbled up unchanged so the host
    /// can decide whether to retry or surface it to the user.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// First four bytes were not `b"RLNC"`. Usually means we are
    /// pointing at someone else's file or a truncated write.
    #[error("magic mismatch: expected RLNC")]
    MagicMismatch,

    /// File header advertises a format version this runtime does not
    /// understand. The host should treat this as a cache miss and
    /// regenerate; we never silently upgrade.
    #[error("version mismatch: file v{file}, runtime v{runtime}")]
    VersionMismatch {
        /// Version stamped in the cache file.
        file: u32,
        /// Version this build of `relon-object-cache` understands.
        runtime: u32,
    },

    /// The cached object was compiled for a different target triple
    /// than the one the host is currently running on (cross-machine
    /// cache directory, architecture migration, …).
    #[error("target triple mismatch: file {file}, runtime {runtime}")]
    TripleMismatch {
        /// Triple recorded in the cache file at write time.
        file: String,
        /// Triple the loader is asking about at read time.
        runtime: String,
    },

    /// HMAC tag at the tail of the file did not validate against the
    /// per-installation key. Treat as hostile until proven otherwise.
    #[error("hmac verification failed")]
    HmacMismatch,

    /// SHA-256 over the object bytes did not match the value embedded
    /// in the filename. Either bit-rot or someone overwrote the body
    /// in place.
    #[error("integrity check failed: sha256 mismatch")]
    Sha256Mismatch,

    /// `bincode` could not decode the metadata trailer. Treated as a
    /// soft miss because layout drift across `Metadata` revisions is
    /// expected.
    #[error("metadata decode: {0}")]
    Metadata(String),

    /// The file is shorter than the smallest possible valid layout.
    /// Almost always means a truncated write or a non-cache file.
    #[error("file too short for a v1 cache blob (got {0} bytes)")]
    Truncated(usize),

    /// Loader was invoked with `IntegrityMode::HmacRequired` but no
    /// HMAC key was supplied. Production callers must always pair the
    /// HMAC-required mode with a present key so a stolen / corrupted
    /// cache file cannot bypass authentication.
    #[error("hmac key required by IntegrityMode::HmacRequired but caller passed None")]
    HmacKeyRequired,
}

/// Errors raised by the `memfd_create` + dlopen path in
/// [`super::loader`].
#[derive(Debug, Error)]
pub enum LoaderError {
    /// `memfd_create(2)` failed (kernel older than 3.17, seccomp,
    /// resource exhaustion, …).
    #[error("memfd_create: {0}")]
    Memfd(std::io::Error),

    /// `write(2)` to the memfd returned an error or a short count.
    #[error("memfd write: {0}")]
    Write(std::io::Error),

    /// `dlopen(3)` rejected the in-memory image. The string carries
    /// the dlerror message verbatim.
    #[error("dlopen: {0}")]
    Dlopen(String),

    /// `dlsym(3)` did not resolve one of the symbols the caller asked
    /// for. Almost always a codegen bug; not user-visible.
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),

    /// The current build target is not Linux. Returned by every
    /// loader entry point on macOS / Windows so the host can fall
    /// back to the JIT path cleanly.
    #[error("platform not supported (gamma phase is Linux-only)")]
    UnsupportedPlatform,
}

/// Errors from the per-installation HMAC key store.
#[derive(Debug, Error)]
pub enum HmacError {
    /// I/O problem touching the key file or its parent directory.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// `getrandom` failed. Very rare; almost always indicates a
    /// kernel without `/dev/urandom`.
    #[error("getrandom: {0}")]
    Random(String),

    /// Key file has wrong size — refuse to use to avoid downgrading
    /// to a partially-zero key.
    #[error("key file has wrong size: expected 32 bytes, got {0}")]
    BadSize(usize),

    /// Key file is world / group readable. Refuse to use rather than
    /// silently downgrade security.
    #[error("key file has insecure mode: 0{0:o} (expected 0600)")]
    InsecureMode(u32),
}
