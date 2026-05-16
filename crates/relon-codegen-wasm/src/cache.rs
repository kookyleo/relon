//! Phase 9.b-3: on-disk AOT cache for compiled wasm modules.
//!
//! Persists the codegen pass's `.wasm` byte output (plus a tiny `.meta`
//! sidecar) so the next host startup can skip parse / analyze / lower
//! / codegen and go straight to `wasmtime::Module::new`. The cache
//! is content-addressed by the sha256 of the source string:
//!
//! ```text
//! <dir>/<source_hash_hex>.wasm     - raw codegen output
//! <dir>/<source_hash_hex>.meta     - magic + versions + schema hash
//! ```
//!
//! Cache validity rules (every mismatch returns `None`, not an error,
//! so callers can fall back to a fresh compile without distinguishing
//! "first run" from "drift"):
//!
//!  * `magic` mismatch → corrupted / unrelated file, treat as miss.
//!  * `format_version` mismatch → forward-compat skip.
//!  * `abi_version` / `codegen_version` mismatch → SDK drift, miss.
//!
//! Not stored (v1 explicit non-goals):
//!
//!  * `wasmtime::Module::serialize` native blobs — those depend on the
//!    cranelift version and target CPU, so caching them across SDK
//!    rebuilds is unsafe. v2 will gate the native blob behind an
//!    "SDK lockstep" handshake.
//!  * Per-call session pool warmup — strictly an in-process artifact.

use crate::abi::{CURRENT_ABI_VERSION, CURRENT_CODEGEN_VERSION};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// 4-byte magic prefix identifying a Relon AOT cache meta file. Distinct
/// from `relon.abi`'s `RLNA` magic so a stray AOT cache file dropped
/// into a different consumer never resolves.
pub const META_MAGIC: [u8; 4] = *b"RLAC";

/// Current binary shape version of the `.meta` file. Bumped whenever the
/// layout below changes; mismatches surface as cache misses.
pub const META_FORMAT_VERSION: u8 = 1;

/// Total encoded size of a `.meta` file in bytes:
///
/// ```text
/// magic              : [u8; 4]                  (4 bytes)
/// format_version     : u8                       (1 byte)
/// abi_version        : u16 LE                   (2 bytes)
/// codegen_version    : u32 LE                   (4 bytes)
/// schema_hash        : [u8; 32]                 (32 bytes)
/// stored_at_unix     : u64 LE                   (8 bytes)
/// ```
pub const META_SIZE: usize = 4 + 1 + 2 + 4 + 32 + 8;

/// Errors raised by [`AotCache`] operations. Read failures that mean
/// "no cached entry" never surface here — `load` returns `Ok(None)` in
/// that case so callers can transparently fall back to a fresh compile.
#[derive(Debug, Error)]
pub enum CacheError {
    /// The cache directory could not be created or was not a directory.
    #[error("cache directory `{path}` is unusable: {source}")]
    DirectoryUnusable {
        /// Configured cache root.
        path: PathBuf,
        /// Underlying I/O failure (typically EEXIST-on-file, EACCES, …).
        #[source]
        source: io::Error,
    },
    /// `store` failed to write the wasm bytes or the meta sidecar. The
    /// path identifies which artifact tripped — useful when diagnosing
    /// half-written cache entries.
    #[error("failed to write cache artifact `{path}`: {source}")]
    Io {
        /// Absolute path of the artifact whose write failed.
        path: PathBuf,
        /// Underlying I/O error from the std fs call.
        #[source]
        source: io::Error,
    },
}

/// One cached compilation, as returned by [`AotCache::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModule {
    /// The exact wasm bytes the codegen pass emitted on the original
    /// store. Re-feed these into `wasmtime::Module::new` to skip the
    /// codegen pipeline; cranelift JIT still runs.
    pub wasm_bytes: Vec<u8>,
    /// Schema fingerprint the host supplied at store time. Hosts use
    /// this to decide whether their current schema matches what the
    /// cached wasm was compiled against; mismatches are treated as
    /// host-level drift (the host re-compiles), not as cache corruption.
    pub schema_hash: [u8; 32],
}

/// A directory-backed AOT compile cache.
///
/// Construction creates the directory if missing (`mkdir -p` semantics).
/// All cache entries live in the configured `dir`; no subdirectory
/// sharding because the source hash already disperses uniformly across
/// the 16^N keyspace and typical hosts cache fewer than a few hundred
/// modules.
#[derive(Debug, Clone)]
pub struct AotCache {
    dir: PathBuf,
}

impl AotCache {
    /// Open (creating if absent) the cache directory at `dir`. Fails
    /// when the path exists but is not a directory, or when the host
    /// cannot create the directory (permissions, parent missing, …).
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, CacheError> {
        let path = dir.as_ref().to_path_buf();
        if let Err(err) = fs::create_dir_all(&path) {
            return Err(CacheError::DirectoryUnusable { path, source: err });
        }
        // Guard against a regular file masquerading as the cache dir
        // (create_dir_all returns Ok when the path already exists, even
        // if it's a regular file — the subsequent write would fail in
        // a much less obvious way).
        let meta = fs::metadata(&path).map_err(|err| CacheError::DirectoryUnusable {
            path: path.clone(),
            source: err,
        })?;
        if !meta.is_dir() {
            return Err(CacheError::DirectoryUnusable {
                path,
                source: io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "cache path exists but is not a directory",
                ),
            });
        }
        Ok(Self { dir: path })
    }

    /// Compute the canonical content-addressed key for `src`. Public
    /// so callers can pre-compute the hash and cache it across `load` /
    /// `store` calls instead of re-hashing the source each time.
    pub fn source_hash(src: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(src.as_bytes());
        hasher.finalize().into()
    }

    /// Look up a cached entry by `source_hash`. Returns `Ok(None)` on
    /// any miss — missing files, magic/version drift, half-written
    /// sidecar — so the caller can transparently fall back to a fresh
    /// compile. Returns `Err` only when the cache directory itself is
    /// in an unrecoverable state (the host cannot recover by re-trying
    /// the read).
    pub fn load(&self, source_hash: [u8; 32]) -> Result<Option<CachedModule>, CacheError> {
        let (wasm_path, meta_path) = self.paths(&source_hash);
        // Fast bail when either side is absent. Cache entries are
        // written in (wasm, meta) order, so a missing wasm always
        // means "never stored" while a missing meta means "torn write"
        // — both classify as miss.
        let wasm_bytes = match fs::read(&wasm_path) {
            Ok(b) => b,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CacheError::Io {
                    path: wasm_path,
                    source: err,
                })
            }
        };
        let meta_bytes = match fs::read(&meta_path) {
            Ok(b) => b,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CacheError::Io {
                    path: meta_path,
                    source: err,
                })
            }
        };
        if meta_bytes.len() != META_SIZE {
            // Truncated / oversized meta — treat as drift.
            return Ok(None);
        }
        if meta_bytes[..4] != META_MAGIC {
            return Ok(None);
        }
        if meta_bytes[4] != META_FORMAT_VERSION {
            return Ok(None);
        }
        let abi_version = u16::from_le_bytes([meta_bytes[5], meta_bytes[6]]);
        if abi_version != CURRENT_ABI_VERSION {
            return Ok(None);
        }
        let codegen_version = u32::from_le_bytes([
            meta_bytes[7],
            meta_bytes[8],
            meta_bytes[9],
            meta_bytes[10],
        ]);
        if codegen_version != CURRENT_CODEGEN_VERSION {
            return Ok(None);
        }
        let mut schema_hash = [0u8; 32];
        schema_hash.copy_from_slice(&meta_bytes[11..43]);
        // stored_at unix timestamp at offset 43..51 is advisory only —
        // we don't use it for invalidation. Future eviction passes can
        // read it without changing the cache surface.
        Ok(Some(CachedModule {
            wasm_bytes,
            schema_hash,
        }))
    }

    /// Persist `wasm_bytes` + `schema_hash` under `source_hash`. Overwrites
    /// any prior entry with the same key (same content always yields
    /// the same hash, so the only collision a sane host can hit is a
    /// re-store of identical bytes).
    ///
    /// Write order is intentionally `(wasm, meta)`: a torn write that
    /// leaves the wasm in place but skips the meta surfaces as a miss
    /// on the next `load`, which is the safe fall-back. The reverse
    /// would re-use the new meta against stale wasm.
    pub fn store(
        &self,
        source_hash: [u8; 32],
        wasm_bytes: &[u8],
        schema_hash: [u8; 32],
    ) -> Result<(), CacheError> {
        let (wasm_path, meta_path) = self.paths(&source_hash);
        fs::write(&wasm_path, wasm_bytes).map_err(|err| CacheError::Io {
            path: wasm_path.clone(),
            source: err,
        })?;
        let meta = encode_meta(schema_hash);
        fs::write(&meta_path, &meta).map_err(|err| CacheError::Io {
            path: meta_path,
            source: err,
        })?;
        Ok(())
    }

    /// Borrow the cache root path. Useful for diagnostics and tests
    /// that want to inspect the on-disk layout.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Compute the wasm + meta paths for a given source hash. Centralised
    /// so future layout changes (sharding, sub-directories) have a
    /// single touch point.
    fn paths(&self, source_hash: &[u8; 32]) -> (PathBuf, PathBuf) {
        let hex = hex_encode(source_hash);
        let mut wasm = self.dir.clone();
        wasm.push(format!("{hex}.wasm"));
        let mut meta = self.dir.clone();
        meta.push(format!("{hex}.meta"));
        (wasm, meta)
    }
}

/// Encode a `.meta` payload for the given schema hash. Stamps the
/// current ABI / codegen versions and the host's wall-clock time at
/// store. Inlined so callers can match against the exact byte layout
/// described in the module docs.
fn encode_meta(schema_hash: [u8; 32]) -> [u8; META_SIZE] {
    let mut out = [0u8; META_SIZE];
    out[..4].copy_from_slice(&META_MAGIC);
    out[4] = META_FORMAT_VERSION;
    out[5..7].copy_from_slice(&CURRENT_ABI_VERSION.to_le_bytes());
    out[7..11].copy_from_slice(&CURRENT_CODEGEN_VERSION.to_le_bytes());
    out[11..43].copy_from_slice(&schema_hash);
    let stored_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    out[43..51].copy_from_slice(&stored_at.to_le_bytes());
    out
}

/// Lowercase hex-encode `bytes` without pulling in an extra dependency.
/// Cache filenames use this directly; the choice of lowercase matches
/// what most CLIs surface when computing sha256 sums.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Build a fresh sub-directory under the system temp dir so tests
    /// running in parallel never clobber each other's cache state.
    fn temp_cache_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut path = std::env::temp_dir();
        path.push(format!("relon-aot-cache-test-{pid}-{nanos}-{counter}-{tag}"));
        path
    }

    #[test]
    fn open_creates_missing_directory() {
        let dir = temp_cache_dir("open");
        assert!(!dir.exists());
        let cache = AotCache::open(&dir).expect("open succeeds");
        assert!(cache.dir().exists());
        assert!(cache.dir().is_dir());
        // Re-open should also work (idempotent mkdir).
        let _again = AotCache::open(&dir).expect("re-open succeeds");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_roundtrip_basic() {
        // Stores wasm + meta, then loads them back through the public
        // load API. Verifies that the round-tripped bytes match what
        // we wrote and that the schema hash survives the trip.
        let dir = temp_cache_dir("roundtrip");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("dummy source");
        let wasm = vec![0u8, 1, 2, 3, 4, 5, 6, 7];
        let schema_hash = [42u8; 32];
        cache
            .store(source_hash, &wasm, schema_hash)
            .expect("store");
        let loaded = cache
            .load(source_hash)
            .expect("load Ok")
            .expect("load hit");
        assert_eq!(loaded.wasm_bytes, wasm);
        assert_eq!(loaded.schema_hash, schema_hash);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_miss_returns_none() {
        // A fresh cache directory must return None for every key —
        // missing entries are not cache errors.
        let dir = temp_cache_dir("miss");
        let cache = AotCache::open(&dir).expect("open");
        let key = AotCache::source_hash("missing source");
        let result = cache.load(key).expect("load Ok");
        assert!(result.is_none(), "cache miss must be None");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn abi_drift_invalidates_cache() {
        // Stamp the meta file with an invalid abi_version. Load must
        // surface None so the host falls back to a fresh compile.
        let dir = temp_cache_dir("abi-drift");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("drift source");
        cache
            .store(source_hash, &[0xde, 0xad, 0xbe, 0xef], [7u8; 32])
            .expect("store");
        // Open the meta sidecar and rewrite abi_version to 99.
        let hex = hex_encode(&source_hash);
        let mut meta_path = dir.clone();
        meta_path.push(format!("{hex}.meta"));
        let mut meta = fs::read(&meta_path).expect("read meta");
        assert_eq!(meta.len(), META_SIZE);
        let bogus: u16 = 99;
        meta[5..7].copy_from_slice(&bogus.to_le_bytes());
        fs::write(&meta_path, &meta).expect("rewrite meta");
        let result = cache.load(source_hash).expect("load Ok");
        assert!(
            result.is_none(),
            "abi_version drift must invalidate cache"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codegen_drift_invalidates_cache() {
        // Same shape as the abi drift test, but flips the codegen
        // version byte instead — a host built against a different
        // codegen revision must not consume cached output from an
        // older codegen.
        let dir = temp_cache_dir("codegen-drift");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("codegen drift source");
        cache
            .store(source_hash, &[1u8, 2, 3], [11u8; 32])
            .expect("store");
        let hex = hex_encode(&source_hash);
        let mut meta_path = dir.clone();
        meta_path.push(format!("{hex}.meta"));
        let mut meta = fs::read(&meta_path).expect("read meta");
        let bogus: u32 = 0xFFFF_FFFF;
        meta[7..11].copy_from_slice(&bogus.to_le_bytes());
        fs::write(&meta_path, &meta).expect("rewrite meta");
        let result = cache.load(source_hash).expect("load Ok");
        assert!(result.is_none(), "codegen_version drift must invalidate");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_meta_treated_as_miss() {
        // A sidecar shorter than META_SIZE means an unfinished /
        // corrupted write — must surface as a miss, not a panic.
        let dir = temp_cache_dir("truncated");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("truncated source");
        cache
            .store(source_hash, &[9u8], [3u8; 32])
            .expect("store");
        let hex = hex_encode(&source_hash);
        let mut meta_path = dir.clone();
        meta_path.push(format!("{hex}.meta"));
        fs::write(&meta_path, &[0u8; 4]).expect("truncate meta");
        let result = cache.load(source_hash).expect("load Ok");
        assert!(result.is_none(), "truncated meta must be a miss");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_hash_is_deterministic_and_disperses() {
        // Same input → same hash; different inputs → different hashes.
        // Cheap regression guard against accidental key-derivation drift.
        let a = AotCache::source_hash("alpha");
        let b = AotCache::source_hash("alpha");
        let c = AotCache::source_hash("beta");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
