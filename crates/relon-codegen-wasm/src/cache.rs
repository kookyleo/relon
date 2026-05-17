//! Phase 9.b-3 / 9.c-1: on-disk AOT cache for compiled wasm modules.
//!
//! Persists the codegen pass's `.wasm` byte output (plus a `.meta`
//! sidecar) so the next host startup can skip parse / analyze / lower
//! / codegen and go straight to `wasmtime::Module::new`. Phase 9.c-1
//! adds an optional `.native` sidecar that stores
//! `wasmtime::Module::serialize`'s cranelift-compiled blob so the
//! load path can call `Module::deserialize` and skip the JIT entirely.
//!
//! The cache is content-addressed by the sha256 of the source string:
//!
//! ```text
//! <dir>/<source_hash_hex>.wasm     - raw codegen output
//! <dir>/<source_hash_hex>.meta     - magic + versions + schema hash
//!                                    + native-compat stamp (v2)
//! <dir>/<source_hash_hex>.schemas  - canonical main/return schemas (JSON,
//!                                    only written by `store_with_schemas`)
//! <dir>/<source_hash_hex>.native   - wasmtime serialized native code
//!                                    (only written by `store_native`)
//! ```
//!
//! Cache validity rules (every mismatch returns `None`, not an error,
//! so callers can fall back to a fresh compile without distinguishing
//! "first run" from "drift"):
//!
//!  * `magic` mismatch → corrupted / unrelated file, treat as miss.
//!  * `format_version` mismatch → forward-compat skip.
//!  * `abi_version` / `codegen_version` mismatch → SDK drift, miss.
//!  * `native_compat_hash` mismatch → wasmtime / target drift, miss
//!    on the `.native` sidecar only (`.wasm` + `.schemas` still load).
//!
//! Native code blob is feature-gated by `Module::deserialize`'s safety
//! requirements:
//!
//!  * Only written by `store_native`, which always pairs a serialised
//!    blob with the `native_compat_hash` covering wasmtime version +
//!    host triple. A mismatching reader returns `None` so the
//!    deserialize path never sees cross-version / cross-arch bytes.
//!  * Loaded through `load_native`, which re-checks the compat hash
//!    before handing the bytes back. Callers wrap the resulting
//!    `Vec<u8>` in an explicit unsafe block when feeding it to
//!    `Module::deserialize`.

use crate::abi::{CURRENT_ABI_VERSION, CURRENT_CODEGEN_VERSION};
use relon_eval_api::schema_canonical::Schema;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use wasmtime::Engine;

/// 4-byte magic prefix identifying a Relon AOT cache meta file. Distinct
/// from `relon.abi`'s `RLNA` magic so a stray AOT cache file dropped
/// into a different consumer never resolves.
pub const META_MAGIC: [u8; 4] = *b"RLAC";

/// Current binary shape version of the `.meta` file. Bumped whenever the
/// layout below changes; mismatches surface as cache misses. v1 → v2
/// added the `native_compat_hash` slot for the `.native` sidecar.
pub const META_FORMAT_VERSION: u8 = 2;

/// Total encoded size of a `.meta` file in bytes (format v2):
///
/// ```text
/// magic              : [u8; 4]                  (4 bytes)
/// format_version     : u8                       (1 byte)
/// abi_version        : u16 LE                   (2 bytes)
/// codegen_version    : u32 LE                   (4 bytes)
/// schema_hash        : [u8; 32]                 (32 bytes)
/// stored_at_unix     : u64 LE                   (8 bytes)
/// native_compat_hash : [u8; 32]                 (32 bytes)
/// ```
///
/// Offsets:
///
/// ```text
///  0..4   magic
///  4..5   format_version
///  5..7   abi_version
///  7..11  codegen_version
/// 11..43  schema_hash
/// 43..51  stored_at_unix
/// 51..83  native_compat_hash
/// ```
pub const META_SIZE: usize = 4 + 1 + 2 + 4 + 32 + 8 + 32;

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
    /// `store_with_schemas` failed to serialise the schemas to JSON,
    /// or `load_with_schemas` failed to parse the schemas sidecar.
    /// Surfaces as a hard error (not a miss) because the host asked for
    /// schemas explicitly — silently dropping back to "no schemas
    /// cached" would hide a real bug in the producer.
    #[error("schema sidecar serde error: {0}")]
    SchemaSerde(String),
}

/// One cached compilation, as returned by [`AotCache::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModule {
    /// The exact wasm bytes the codegen pass emitted on the original
    /// store. Re-feed these into `wasmtime::Module::new` to skip the
    /// codegen pipeline; cranelift JIT still runs unless the native
    /// sidecar is also available.
    pub wasm_bytes: Vec<u8>,
    /// Schema fingerprint the host supplied at store time. Hosts use
    /// this to decide whether their current schema matches what the
    /// cached wasm was compiled against; mismatches are treated as
    /// host-level drift (the host re-compiles), not as cache corruption.
    pub schema_hash: [u8; 32],
    /// Optional canonical `(main, return)` schemas, populated only when
    /// the entry was written through [`AotCache::store_with_schemas`].
    /// `from_source_with_cache` needs both schemas on a hit to bypass
    /// re-running the analyzer / lowering pipeline, so the convenience
    /// constructor pairs the wasm bytes with the canonical schemas in
    /// one go.
    pub schemas: Option<CachedSchemas>,
    /// Native-compatibility stamp persisted in the meta sidecar. Carries
    /// the wasmtime version + target triple fingerprint the entry was
    /// originally stored under. Consumers use this to decide whether a
    /// `.native` sidecar (loaded separately via
    /// [`AotCache::load_native`]) is usable on the current host.
    pub native_compat_hash: [u8; 32],
}

/// A pre-validated chunk of cranelift-serialised native code returned
/// by [`AotCache::load_native`]. Holding this value means the meta
/// sidecar's `native_compat_hash` matched the current host's
/// fingerprint at read time — the bytes are still untrusted input from
/// the caller's perspective (corruption, hostile editing) so feeding
/// them to `wasmtime::Module::deserialize` remains an `unsafe`
/// operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedNative {
    /// Output of `wasmtime::Module::serialize` written by
    /// [`AotCache::store_native`]. Treat as opaque bytes — the format
    /// is wasmtime-private and we never inspect it.
    pub bytes: Vec<u8>,
}

/// Canonical `(main, return)` schema pair persisted alongside the wasm
/// bytes. Stored as JSON in a separate sidecar so the binary `.meta`
/// shape stays fixed-size and the parser side stays simple.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CachedSchemas {
    /// Canonical `#main` parameter schema as recorded by the codegen
    /// pipeline at store time.
    pub main: Schema,
    /// Canonical return schema.
    pub return_: Schema,
}

/// A directory-backed AOT compile cache.
///
/// Construction creates the directory if missing (`mkdir -p` semantics).
/// All cache entries live in the configured `dir`; no subdirectory
/// sharding because the source hash already disperses uniformly across
/// the 16^N keyspace and typical hosts cache fewer than a few hundred
/// modules.
///
/// Phase 9.c-2: the cache also owns a [`wasmtime::Engine`]. wasmtime's
/// `Engine` is reference-counted internally (clone is a cheap `Arc`
/// bump), and constructing one runs a non-trivial amount of setup
/// (Config defaults, cranelift settings, target ISA detection) that
/// historically dominated the cached cold-start budget at ~50-100 μs
/// per construction. Folding the engine into the cache lets every
/// evaluator built through this cache share the same Engine without
/// adding a separate global. Hosts that want a custom Config can
/// supply their own engine through [`Self::with_engine`].
#[derive(Debug, Clone)]
pub struct AotCache {
    dir: PathBuf,
    /// Shared wasmtime compilation engine. Cloning an `Engine` is a
    /// cheap `Arc::clone` — the actual cranelift / Config state lives
    /// in a single shared allocation. Held by value because every
    /// `Engine::clone` callsite needs an `Engine`, not an `&Engine`,
    /// to feed `wasmtime::Module::new` / `Store::new`.
    engine: Engine,
}

impl AotCache {
    /// Open (creating if absent) the cache directory at `dir`. Fails
    /// when the path exists but is not a directory, or when the host
    /// cannot create the directory (permissions, parent missing, …).
    ///
    /// Pairs the cache with a default-configured [`wasmtime::Engine`].
    /// Use [`Self::with_engine`] to supply a host-tuned engine.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, CacheError> {
        Self::open_with_engine(dir, Engine::default())
    }

    /// Open (creating if absent) the cache directory at `dir`, binding
    /// the cache to a caller-supplied [`wasmtime::Engine`]. Useful when
    /// the host needs a non-default `wasmtime::Config` (custom
    /// allocator, profiling hooks, …) — all evaluators built through
    /// this cache will share that engine.
    ///
    /// Wasmtime requires that `Module::deserialize` and `Module::new`
    /// be paired with the same engine that produced the bytes. The
    /// cache itself does not stamp engine fingerprints (only the
    /// wasmtime version + host triple via `native_compat_hash`), so
    /// hosts that swap engine config across cache writers and readers
    /// must invalidate the on-disk `.native` sidecars themselves.
    pub fn open_with_engine(dir: impl AsRef<Path>, engine: Engine) -> Result<Self, CacheError> {
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
        Ok(Self { dir: path, engine })
    }

    /// Replace the wasmtime engine bound to this cache. Returns the
    /// updated cache so the builder pattern composes with `open`.
    ///
    /// Note: existing evaluators already constructed off this cache
    /// hold their own engine clone, so swapping the engine here does
    /// not retroactively retarget them. Use this hook before the first
    /// `WasmAotEvaluator::from_source_with_cache` call.
    pub fn with_engine(mut self, engine: Engine) -> Self {
        self.engine = engine;
        self
    }

    /// Borrow the wasmtime engine the cache shares with every
    /// evaluator built through it. Hosts that need to instantiate a
    /// `Module` outside `WasmAotEvaluator` (custom embedding,
    /// diagnostic dumps) can reuse the same engine and avoid paying
    /// the engine setup cost twice.
    pub fn engine(&self) -> &Engine {
        &self.engine
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
        let Some(parsed) = parse_meta(&meta_bytes) else {
            // Truncated / oversized / drifted meta — treat as miss.
            return Ok(None);
        };

        // The schemas sidecar is optional. Producers that only know the
        // hash (e.g. tooling that fingerprints third-party wasm) skip
        // the file; `from_source_with_cache`-style consumers write it
        // so a hit can short-circuit parse / analyze / lowering.
        let schemas_path = self.schemas_path(&source_hash);
        let schemas = match fs::read(&schemas_path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<CachedSchemas>(&bytes)
                    .map_err(|e| CacheError::SchemaSerde(e.to_string()))?,
            ),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(CacheError::Io {
                    path: schemas_path,
                    source: err,
                })
            }
        };

        Ok(Some(CachedModule {
            wasm_bytes,
            schema_hash: parsed.schema_hash,
            schemas,
            native_compat_hash: parsed.native_compat_hash,
        }))
    }

    /// Look up the cranelift native-code sidecar for `source_hash`.
    /// Returns `Ok(None)` on:
    ///
    ///  * no `.native` file present (entry was stored before native
    ///    caching kicked in, or `store_native` was never called),
    ///  * meta sidecar missing / drifted (treated as a clean miss),
    ///  * meta sidecar's `native_compat_hash` does not match the
    ///    current host's fingerprint — i.e. the binary was produced
    ///    by a wasmtime version or for a target that the running
    ///    process cannot safely consume.
    ///
    /// On success the returned [`CachedNative`] wraps the raw bytes
    /// suitable to hand to `wasmtime::Module::deserialize`. The caller
    /// is still responsible for the `unsafe` invocation — this method
    /// only guarantees the bytes were produced under the same
    /// wasmtime version and host triple as the current process.
    pub fn load_native(&self, source_hash: [u8; 32]) -> Result<Option<CachedNative>, CacheError> {
        let meta_path = self.meta_path(&source_hash);
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
        let Some(parsed) = parse_meta(&meta_bytes) else {
            return Ok(None);
        };
        // Native-compat drift: wasmtime version / target triple
        // mismatch. Surface as a clean miss so the caller falls back
        // to JIT'ing the wasm side without distinguishing "never had
        // a native sidecar" from "had one but invalidated".
        if parsed.native_compat_hash != current_native_compat_hash() {
            return Ok(None);
        }
        let native_path = self.native_path(&source_hash);
        let native_bytes = match fs::read(&native_path) {
            Ok(b) => b,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CacheError::Io {
                    path: native_path,
                    source: err,
                })
            }
        };
        Ok(Some(CachedNative {
            bytes: native_bytes,
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
        // Delete any stale `.schemas` / `.native` sidecars so a
        // subsequent schemaless `store` followed by `load` does not
        // silently return mismatched schemas / pre-existing native code
        // tied to the previous wasm content.
        let schemas_path = self.schemas_path(&source_hash);
        remove_if_present(&schemas_path)?;
        let native_path = self.native_path(&source_hash);
        remove_if_present(&native_path)?;
        fs::write(&wasm_path, wasm_bytes).map_err(|err| CacheError::Io {
            path: wasm_path.clone(),
            source: err,
        })?;
        let meta = encode_meta(schema_hash);
        fs::write(&meta_path, meta).map_err(|err| CacheError::Io {
            path: meta_path,
            source: err,
        })?;
        Ok(())
    }

    /// Persist `wasm_bytes` + `schema_hash` + canonical `(main, return)`
    /// schemas under `source_hash`. Mirrors [`Self::store`] but
    /// additionally writes a `.schemas` JSON sidecar so
    /// `WasmAotEvaluator::from_source_with_cache` can rebuild the
    /// evaluator on a hit without re-running the analyzer / lowering
    /// pipeline.
    ///
    /// Write order: wasm → schemas → meta. A torn write that drops
    /// the meta sidecar surfaces as a miss (per `load`'s rules), so
    /// the cache transparently re-stores rather than handing back a
    /// half-written entry.
    pub fn store_with_schemas(
        &self,
        source_hash: [u8; 32],
        wasm_bytes: &[u8],
        schema_hash: [u8; 32],
        schemas: &CachedSchemas,
    ) -> Result<(), CacheError> {
        let (wasm_path, meta_path) = self.paths(&source_hash);
        let schemas_path = self.schemas_path(&source_hash);
        // Clear any stale `.native` for the previous content under the
        // same key. Re-stamping `.wasm` invalidates any previously
        // compiled native blob anyway, and leaving it around would
        // create a window where `load_native` returns bytes that
        // disagree with the freshly stored wasm.
        let native_path = self.native_path(&source_hash);
        remove_if_present(&native_path)?;
        fs::write(&wasm_path, wasm_bytes).map_err(|err| CacheError::Io {
            path: wasm_path.clone(),
            source: err,
        })?;
        let schemas_json =
            serde_json::to_vec(schemas).map_err(|e| CacheError::SchemaSerde(e.to_string()))?;
        fs::write(&schemas_path, &schemas_json).map_err(|err| CacheError::Io {
            path: schemas_path,
            source: err,
        })?;
        let meta = encode_meta(schema_hash);
        fs::write(&meta_path, meta).map_err(|err| CacheError::Io {
            path: meta_path,
            source: err,
        })?;
        Ok(())
    }

    /// Persist `native_bytes` (output of `wasmtime::Module::serialize`)
    /// as the `.native` sidecar for `source_hash`. The meta sidecar
    /// stamped by an earlier `store_*` call already carries the host's
    /// native-compat hash; this method only writes the binary blob, so
    /// it presumes the caller has already produced a matching
    /// `.wasm` / `.meta` pair (otherwise `load_native` would surface
    /// `None` on the next read).
    ///
    /// The write is best-effort idempotent: callers that race to write
    /// the same key both end up with byte-identical content because
    /// `Module::serialize` is deterministic for a given
    /// (wasmtime version, target, wasm input) triple.
    pub fn store_native(
        &self,
        source_hash: [u8; 32],
        native_bytes: &[u8],
    ) -> Result<(), CacheError> {
        let native_path = self.native_path(&source_hash);
        fs::write(&native_path, native_bytes).map_err(|err| CacheError::Io {
            path: native_path,
            source: err,
        })
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
        (self.wasm_path(source_hash), self.meta_path(source_hash))
    }

    fn wasm_path(&self, source_hash: &[u8; 32]) -> PathBuf {
        let hex = hex_encode(source_hash);
        let mut p = self.dir.clone();
        p.push(format!("{hex}.wasm"));
        p
    }

    fn meta_path(&self, source_hash: &[u8; 32]) -> PathBuf {
        let hex = hex_encode(source_hash);
        let mut p = self.dir.clone();
        p.push(format!("{hex}.meta"));
        p
    }

    /// Compute the schemas sidecar path for a given source hash. The
    /// sidecar is optional — only producers that need to rehydrate the
    /// canonical schemas at load time write it.
    fn schemas_path(&self, source_hash: &[u8; 32]) -> PathBuf {
        let hex = hex_encode(source_hash);
        let mut p = self.dir.clone();
        p.push(format!("{hex}.schemas"));
        p
    }

    /// Compute the native-code sidecar path for a given source hash.
    /// Carries `wasmtime::Module::serialize` output when the producer
    /// invoked [`Self::store_native`]; otherwise absent.
    fn native_path(&self, source_hash: &[u8; 32]) -> PathBuf {
        let hex = hex_encode(source_hash);
        let mut p = self.dir.clone();
        p.push(format!("{hex}.native"));
        p
    }
}

/// Decoded view of a `.meta` sidecar. Internal only; the public surface
/// returns the relevant pieces through [`CachedModule`] /
/// [`AotCache::load_native`].
struct ParsedMeta {
    schema_hash: [u8; 32],
    native_compat_hash: [u8; 32],
}

/// Parse `bytes` as a meta sidecar, returning `None` for any drift /
/// corruption signal. Centralised so `load` and `load_native` apply
/// identical validity rules.
fn parse_meta(bytes: &[u8]) -> Option<ParsedMeta> {
    if bytes.len() != META_SIZE {
        return None;
    }
    if bytes[..4] != META_MAGIC {
        return None;
    }
    if bytes[4] != META_FORMAT_VERSION {
        return None;
    }
    let abi_version = u16::from_le_bytes([bytes[5], bytes[6]]);
    if abi_version != CURRENT_ABI_VERSION {
        return None;
    }
    let codegen_version = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
    if codegen_version != CURRENT_CODEGEN_VERSION {
        return None;
    }
    let mut schema_hash = [0u8; 32];
    schema_hash.copy_from_slice(&bytes[11..43]);
    // stored_at unix timestamp at offset 43..51 is advisory only —
    // we don't use it for invalidation. Future eviction passes can
    // read it without changing the cache surface.
    let mut native_compat_hash = [0u8; 32];
    native_compat_hash.copy_from_slice(&bytes[51..83]);
    Some(ParsedMeta {
        schema_hash,
        native_compat_hash,
    })
}

/// Encode a `.meta` payload for the given schema hash. Stamps the
/// current ABI / codegen versions, the host's wall-clock time at
/// store, and the native-compat hash (wasmtime version + target triple)
/// so consumers can decide whether a `.native` sidecar is usable.
/// Inlined so callers can match against the exact byte layout described
/// in the module docs.
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
    out[51..83].copy_from_slice(&current_native_compat_hash());
    out
}

/// Wasmtime crate version string the cache was compiled against.
/// Stamped into the native-compat hash so a host built against a
/// newer wasmtime can never deserialise a blob produced by an older
/// one (wasmtime itself also rejects cross-version blobs at the
/// `Module::deserialize` boundary; the stamp gives us a fast pre-load
/// reject so we don't pay for the deserialize attempt).
const WASMTIME_VERSION_TAG: &str = "wasmtime-44";

/// Compose the 32-byte fingerprint that gates `.native` sidecar reuse.
/// Inputs:
///
///  * `WASMTIME_VERSION_TAG` — pinned in-tree so a wasmtime bump
///    invalidates every cached native blob without needing a separate
///    schema migration.
///  * `std::env::consts::ARCH` / `OS` / `FAMILY` — host triple
///    components available without a build-time `cargo:rerun-if`
///    plumbing. Cranelift-emitted code is ISA + ABI specific, so a
///    cache directory shared across machines (e.g. NFS-mounted) must
///    differentiate per host shape.
///  * `usize::BITS` — guards against 32-bit / 64-bit confusion in the
///    unlikely event the triple components alone aren't enough.
fn current_native_compat_hash() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(WASMTIME_VERSION_TAG.as_bytes());
    hasher.update(b"|arch=");
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(b"|os=");
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(b"|family=");
    hasher.update(std::env::consts::FAMILY.as_bytes());
    hasher.update(b"|bits=");
    hasher.update((usize::BITS).to_le_bytes());
    hasher.finalize().into()
}

/// Remove a cache sidecar if it exists, treating NotFound as success.
/// Surfaces any other I/O failure as [`CacheError::Io`] so half-written
/// states caused by transient filesystem errors don't go unnoticed.
fn remove_if_present(path: &Path) -> Result<(), CacheError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CacheError::Io {
            path: path.to_path_buf(),
            source: err,
        }),
    }
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
        path.push(format!(
            "relon-aot-cache-test-{pid}-{nanos}-{counter}-{tag}"
        ));
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
        cache.store(source_hash, &wasm, schema_hash).expect("store");
        let loaded = cache.load(source_hash).expect("load Ok").expect("load hit");
        assert_eq!(loaded.wasm_bytes, wasm);
        assert_eq!(loaded.schema_hash, schema_hash);
        // Plain `store` does not write the schemas sidecar.
        assert!(loaded.schemas.is_none());
        // But meta always carries the current host's native-compat
        // stamp so subsequent `store_native` / `load_native` calls can
        // round-trip a cranelift blob without re-writing the meta.
        assert_eq!(loaded.native_compat_hash, current_native_compat_hash());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_roundtrip_with_schemas() {
        // store_with_schemas additionally persists canonical (main,
        // return) schemas so `load` returns Some(CachedSchemas).
        use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
        let dir = temp_cache_dir("roundtrip-schemas");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("schema source");
        let wasm = vec![1u8, 2, 3, 4];
        let schema_hash = [5u8; 32];
        let schemas = CachedSchemas {
            main: Schema {
                name: "MainParams".into(),
                generics: vec![],
                fields: vec![Field {
                    name: "x".into(),
                    ty: TypeRepr::Int,
                    default: None,
                }],
            },
            return_: Schema {
                name: "MainReturn".into(),
                generics: vec![],
                fields: vec![Field {
                    name: "value".into(),
                    ty: TypeRepr::Int,
                    default: None,
                }],
            },
        };
        cache
            .store_with_schemas(source_hash, &wasm, schema_hash, &schemas)
            .expect("store_with_schemas");
        let loaded = cache.load(source_hash).expect("load Ok").expect("load hit");
        assert_eq!(loaded.wasm_bytes, wasm);
        assert_eq!(loaded.schema_hash, schema_hash);
        assert_eq!(loaded.schemas.expect("schemas survived"), schemas);
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
        // load_native on a missing key must also be Ok(None), not Err.
        let nat = cache.load_native(key).expect("load_native Ok");
        assert!(nat.is_none(), "native miss must be None");
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
        assert!(result.is_none(), "abi_version drift must invalidate cache");
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
        cache.store(source_hash, &[9u8], [3u8; 32]).expect("store");
        let hex = hex_encode(&source_hash);
        let mut meta_path = dir.clone();
        meta_path.push(format!("{hex}.meta"));
        fs::write(&meta_path, [0u8; 4]).expect("truncate meta");
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

    #[test]
    fn native_roundtrip_basic() {
        // store_native writes the `.native` sidecar; load_native
        // reads it back when the meta's compat hash matches the
        // current host (which it does for any sidecar produced by
        // this process — encode_meta stamps the live hash).
        let dir = temp_cache_dir("native");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("native source");
        cache
            .store(source_hash, &[1u8, 2, 3, 4], [9u8; 32])
            .expect("store");
        let blob = vec![0xde, 0xad, 0xbe, 0xef, 0x42, 0x42];
        cache
            .store_native(source_hash, &blob)
            .expect("store_native");
        let loaded = cache
            .load_native(source_hash)
            .expect("load_native Ok")
            .expect("native hit");
        assert_eq!(loaded.bytes, blob);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_invalidated_by_compat_hash_drift() {
        // Tamper with the meta's native_compat_hash so load_native
        // returns None on a still-present .native sidecar. The .wasm
        // path stays loadable — only the native side is invalidated.
        let dir = temp_cache_dir("native-drift");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("native drift source");
        cache
            .store(source_hash, &[5u8, 5, 5], [1u8; 32])
            .expect("store");
        cache
            .store_native(source_hash, &[0xaa; 16])
            .expect("store_native");
        // Rewrite the native_compat_hash slot to zero — a guaranteed
        // mismatch against the current host.
        let hex = hex_encode(&source_hash);
        let mut meta_path = dir.clone();
        meta_path.push(format!("{hex}.meta"));
        let mut meta = fs::read(&meta_path).expect("read meta");
        for byte in meta[51..83].iter_mut() {
            *byte = 0;
        }
        fs::write(&meta_path, &meta).expect("rewrite meta");
        let nat = cache.load_native(source_hash).expect("load_native Ok");
        assert!(nat.is_none(), "compat drift must invalidate native cache");
        // The .wasm side is still loadable — only the native sidecar
        // got invalidated. The schema-hash equality survives intact.
        let module = cache.load(source_hash).expect("load Ok").expect("hit");
        assert_eq!(module.schema_hash, [1u8; 32]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_missing_returns_none() {
        // store (no native) → load_native must report None without
        // reading any phantom sidecar.
        let dir = temp_cache_dir("native-missing");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("native missing source");
        cache.store(source_hash, &[7u8], [2u8; 32]).expect("store");
        let nat = cache.load_native(source_hash).expect("load_native Ok");
        assert!(nat.is_none(), "no .native sidecar → miss");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_clears_stale_native_sidecar() {
        // Re-storing the wasm bytes under a key must drop any
        // previously written `.native` sidecar so a subsequent
        // load_native does not feed wasmtime stale machine code.
        let dir = temp_cache_dir("native-clear");
        let cache = AotCache::open(&dir).expect("open");
        let source_hash = AotCache::source_hash("native clear source");
        cache.store(source_hash, &[1u8], [4u8; 32]).expect("store");
        cache
            .store_native(source_hash, &[0xbb; 8])
            .expect("store_native");
        // Re-issue `store` — the prior `.native` blob must be cleared.
        cache
            .store(source_hash, &[2u8], [4u8; 32])
            .expect("re-store");
        let nat = cache.load_native(source_hash).expect("load_native Ok");
        assert!(
            nat.is_none(),
            "re-store must drop the stale .native sidecar"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_native_compat_hash_is_deterministic() {
        // Repeated calls inside one process must agree, otherwise we'd
        // invalidate every cache entry across two reads.
        let a = current_native_compat_hash();
        let b = current_native_compat_hash();
        assert_eq!(a, b);
    }
}
