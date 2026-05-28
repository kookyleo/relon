//! On-disk cache file format and read / write helpers.
//!
//! ## File layout (little-endian throughout)
//!
//! | offset | size       | field                          |
//! |--------|------------|--------------------------------|
//! |   0    | 4          | magic = `b"RLNC"`              |
//! |   4    | 4          | version `u32` = [`CACHE_VERSION`] |
//! |   8    | 1          | `triple_len: u8`               |
//! |   9    | `triple_len` | `target_triple` (ASCII)      |
//! | 9 + t  | 4          | `object_size: u32`             |
//! | 13 + t | `object_size` | object bytes (ELF / Mach-O) |
//! | 13 + t + o | 4      | `metadata_size: u32`           |
//! | 17 + t + o | `metadata_size` | bincode `Metadata` |
//! | END - 32 | 32       | HMAC-SHA256 over bytes [0..END-32] (zeros when HMAC disabled) |
//!
//! The trailing 32 bytes are **always** present so the layout is
//! constant — the host distinguishes HMAC-protected files from
//! plain ones via the `hmac_key` argument, not by inspecting the
//! tail. This means a file written without HMAC can later be
//! verified by computing HMAC over the zero-tag region without
//! shifting offsets.
//!
//! ## Filename
//!
//! `<sha256_hex>.relon-native-v1` — the SHA-256 of the *source*
//! (canonical IR + caps + signature; the codegen side decides what
//! goes in) is supplied by the caller and used verbatim as the
//! filename stem. This crate never inspects the source; it just
//! stores what it is given.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::CacheError;
use crate::hmac::{compute_hmac, verify_hmac};
use crate::integrity::IntegrityMode;

/// Magic prefix — `R`elon `N`ative `C`ache.
pub const CACHE_MAGIC: [u8; 4] = *b"RLNC";

/// On-disk layout version. Bump on any incompatible change; readers
/// reject every other value with [`CacheError::VersionMismatch`].
pub const CACHE_VERSION: u32 = 1;

/// Length in bytes of the trailing HMAC tag (SHA-256 output size).
pub const HMAC_TAG_LEN: usize = 32;

/// Filename suffix shared by every cache blob.
pub const CACHE_FILE_SUFFIX: &str = ".relon-native-v1";

/// Per-import metadata. Captures the host-fn name, the capability
/// bit it draws from, and parameter / return-type fingerprints so
/// the loader can refuse a cache that targets a different ABI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostFnImport {
    /// Symbol name as emitted by cranelift-object, e.g.
    /// `"relon_host_log"`.
    pub name: String,
    /// Capability bit (0..63) this import draws from.
    pub cap_bit: u32,
    /// SHA-256 of the canonical parameter ABI string.
    pub params_hash: [u8; 32],
    /// SHA-256 of the canonical return-type ABI string.
    pub returns_hash: [u8; 32],
}

/// Signature digest for the `#main` entry point. Kept as an opaque
/// 32-byte fingerprint so the storage layer does not need to
/// understand the IR-level type system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureHash(pub [u8; 32]);

/// Cache-trailer payload. Everything the loader / sandbox needs in
/// order to validate that the object is still compatible with the
/// current runtime environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    /// Imports the object references (host fns + stdlib).
    pub host_fn_imports: Vec<HostFnImport>,
    /// Bit-mask of capabilities the source declared.
    pub cap_bitmap: u64,
    /// Signature digest of the `#main` entry point.
    pub main_signature: SignatureHash,
    /// Unix epoch seconds at which the cache was written; advisory
    /// only (used by GC heuristics, never by correctness checks).
    pub created_at_unix: u64,
    /// Free-form generator stamp — typically
    /// `"relon-codegen-cranelift <semver>"`.
    pub generator_version: String,
}

/// In-memory view of a cache hit. The object bytes are eagerly
/// copied so the file handle can be released immediately.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Target triple the object was compiled for.
    pub target_triple: String,
    /// Raw ELF / Mach-O bytes, ready to hand to the loader.
    pub object_bytes: Vec<u8>,
    /// Metadata trailer.
    pub metadata: Metadata,
}

/// Lower-case hex encoding of a SHA-256 digest, no `0x` prefix.
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `write!` into `String` is infallible so the result is
        // ignored intentionally.
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}

/// Compose the canonical path: `<cache_dir>/<sha_hex><SUFFIX>`.
pub fn cache_path_for(cache_dir: &Path, source_sha256: [u8; 32]) -> PathBuf {
    let mut name = hex_lower(&source_sha256);
    name.push_str(CACHE_FILE_SUFFIX);
    cache_dir.join(name)
}

/// Serialize the four sections — header, object, metadata, HMAC tag
/// — into a single `Vec<u8>`. Pulled out so the test suite can
/// poke individual offsets without touching the filesystem.
pub(crate) fn encode_blob(
    target_triple: &str,
    object_bytes: &[u8],
    metadata: &Metadata,
    hmac_key: Option<&[u8; 32]>,
) -> Result<Vec<u8>, CacheError> {
    let triple_bytes = target_triple.as_bytes();
    if triple_bytes.len() > u8::MAX as usize {
        return Err(CacheError::Metadata(format!(
            "target triple too long: {} bytes",
            triple_bytes.len()
        )));
    }
    if object_bytes.len() > u32::MAX as usize {
        return Err(CacheError::Metadata(format!(
            "object too large: {} bytes",
            object_bytes.len()
        )));
    }

    let meta_bytes = bincode::serialize(metadata)
        .map_err(|e| CacheError::Metadata(format!("bincode serialize: {e}")))?;
    if meta_bytes.len() > u32::MAX as usize {
        return Err(CacheError::Metadata(format!(
            "metadata too large: {} bytes",
            meta_bytes.len()
        )));
    }

    let mut buf = Vec::with_capacity(
        4 + 4
            + 1
            + triple_bytes.len()
            + 4
            + object_bytes.len()
            + 4
            + meta_bytes.len()
            + HMAC_TAG_LEN,
    );
    buf.extend_from_slice(&CACHE_MAGIC);
    buf.extend_from_slice(&CACHE_VERSION.to_le_bytes());
    buf.push(triple_bytes.len() as u8);
    buf.extend_from_slice(triple_bytes);
    buf.extend_from_slice(&(object_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(object_bytes);
    buf.extend_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(&meta_bytes);

    // Pre-fill the HMAC slot with zeros so the layout is constant
    // regardless of whether a key was supplied. We compute the tag
    // over everything *before* the slot.
    let body_end = buf.len();
    buf.resize(body_end + HMAC_TAG_LEN, 0);

    if let Some(key) = hmac_key {
        let tag = compute_hmac(&buf[..body_end], key);
        buf[body_end..].copy_from_slice(&tag);
    }
    Ok(buf)
}

/// Write a cache blob to `<cache_dir>/<sha_hex>.relon-native-v1`,
/// using an atomic-rename strategy so concurrent producers cannot
/// observe a partial file.
///
/// `target_triple` is recorded inside the blob, **not** the
/// filename: two builds for different triples that happen to hash
/// to the same source will collide on the filename — that is the
/// caller's responsibility to disambiguate by hashing the triple
/// into `source_sha256`.
pub fn store(
    cache_dir: &Path,
    source_sha256: [u8; 32],
    target_triple: &str,
    object_bytes: &[u8],
    metadata: &Metadata,
    hmac_key: Option<&[u8; 32]>,
) -> Result<PathBuf, CacheError> {
    fs::create_dir_all(cache_dir)?;
    let final_path = cache_path_for(cache_dir, source_sha256);

    let blob = encode_blob(target_triple, object_bytes, metadata, hmac_key)?;

    // PID + nanosecond suffix avoids collisions when multiple
    // producers race on the same source hash. `rename(2)` is atomic
    // within a single filesystem, so the worst case is "the last
    // writer wins" — no half-written file is ever visible.
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp = final_path.with_extension(format!("tmp.{}.{}", pid, nonce));

    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&blob)?;
        f.flush()?;
    }
    fs::rename(&tmp, &final_path)?;
    Ok(final_path)
}

/// Load and validate a cache entry. Returns `Ok(None)` only when
/// the file is absent; every other reason (corruption, version
/// skew, triple mismatch, HMAC failure, …) surfaces as a typed
/// [`CacheError`] so the caller can log it and fall back to
/// regenerating the cache.
///
/// `expected_triple` is checked against the value stored in the
/// blob; pass the loader's current host triple.
///
/// `integrity` decides how the loader proves the cache file's
/// integrity. See [`IntegrityMode`] for the trade-offs.
/// - [`IntegrityMode::Strict`] (default) — always re-hash.
/// - [`IntegrityMode::HmacRequired`] — refuse to load with no HMAC
///   key; rely on the HMAC tag (covers header + object bytes +
///   metadata) for tamper detection. Strict's SHA-256 recompute is
///   skipped because the filename stem is a source-derived key, not
///   the object body's own hash.
pub fn load(
    cache_dir: &Path,
    source_sha256: [u8; 32],
    expected_triple: &str,
    hmac_key: Option<&[u8; 32]>,
    integrity: IntegrityMode,
) -> Result<Option<CacheEntry>, CacheError> {
    // Refuse the HMAC-required mode early when no key is supplied:
    // we must not silently downgrade to a no-authentication load
    // even if the on-disk blob also happens to have a zero HMAC
    // trailer (writer dropped to `hmac_key = None` is exactly the
    // bypass we're guarding against here).
    integrity.enforce_hmac_present(hmac_key)?;

    let path = cache_path_for(cache_dir, source_sha256);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let entry = decode_blob(&bytes, expected_triple, hmac_key)?;

    if integrity == IntegrityMode::Strict {
        let mut hasher = Sha256::new();
        hasher.update(&entry.object_bytes);
        let actual: [u8; 32] = hasher.finalize().into();
        if actual != source_sha256 {
            return Err(CacheError::Sha256Mismatch);
        }
    }
    Ok(Some(entry))
}

/// Decode a fully-buffered blob. Pulled out so unit tests can hand
/// in synthetic byte streams without touching the filesystem.
pub(crate) fn decode_blob(
    bytes: &[u8],
    expected_triple: &str,
    hmac_key: Option<&[u8; 32]>,
) -> Result<CacheEntry, CacheError> {
    // Minimum layout: magic(4) + ver(4) + triple_len(1) + obj_size(4)
    // + meta_size(4) + hmac(32) = 49 bytes. Reject anything smaller
    // up-front so the slice operations below cannot underflow.
    const MIN_LEN: usize = 4 + 4 + 1 + 4 + 4 + HMAC_TAG_LEN;
    if bytes.len() < MIN_LEN {
        return Err(CacheError::Truncated(bytes.len()));
    }

    if bytes[0..4] != CACHE_MAGIC {
        return Err(CacheError::MagicMismatch);
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != CACHE_VERSION {
        return Err(CacheError::VersionMismatch {
            file: version,
            runtime: CACHE_VERSION,
        });
    }

    let triple_len = bytes[8] as usize;
    let triple_end = 9 + triple_len;
    if triple_end + 4 > bytes.len() {
        return Err(CacheError::Truncated(bytes.len()));
    }
    let triple = std::str::from_utf8(&bytes[9..triple_end])
        .map_err(|e| CacheError::Metadata(format!("triple utf8: {e}")))?
        .to_owned();
    if triple != expected_triple {
        return Err(CacheError::TripleMismatch {
            file: triple,
            runtime: expected_triple.to_owned(),
        });
    }

    let obj_size =
        u32::from_le_bytes(bytes[triple_end..triple_end + 4].try_into().unwrap()) as usize;
    let obj_start = triple_end + 4;
    let obj_end = obj_start
        .checked_add(obj_size)
        .ok_or(CacheError::Truncated(bytes.len()))?;
    if obj_end + 4 > bytes.len() {
        return Err(CacheError::Truncated(bytes.len()));
    }
    let object_bytes = bytes[obj_start..obj_end].to_vec();

    let meta_size = u32::from_le_bytes(bytes[obj_end..obj_end + 4].try_into().unwrap()) as usize;
    let meta_start = obj_end + 4;
    let meta_end = meta_start
        .checked_add(meta_size)
        .ok_or(CacheError::Truncated(bytes.len()))?;
    if meta_end + HMAC_TAG_LEN != bytes.len() {
        return Err(CacheError::Truncated(bytes.len()));
    }
    let metadata: Metadata = bincode::deserialize(&bytes[meta_start..meta_end])
        .map_err(|e| CacheError::Metadata(format!("bincode: {e}")))?;

    if let Some(key) = hmac_key {
        let mut expected = [0u8; HMAC_TAG_LEN];
        expected.copy_from_slice(&bytes[meta_end..]);
        if !verify_hmac(&bytes[..meta_end], key, &expected) {
            return Err(CacheError::HmacMismatch);
        }
    }

    Ok(CacheEntry {
        target_triple: triple,
        object_bytes,
        metadata,
    })
}
