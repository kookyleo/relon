//! v5-γ stage 2 schema cache.
//!
//! Sidekick file persisted alongside the relon-object-cache ET_DYN +
//! the legacy IR cache. The schema cache holds the bits of context the
//! buffer-protocol trampoline needs that the object cache does not
//! already expose: the canonical main / return schemas, the const-data
//! blob, the entry shape + range, parameter names, and the closure
//! count. Without these, the dlopen-exec hot path would need to
//! re-parse + re-analyze the source on every cold start (~50-100 µs),
//! which blows the v5-γ stage 2 strict-mode budget of ≤ 15 µs.
//!
//! ## File layout
//!
//! Filename: `<source_hash>.relon-schema-v1`, alongside
//! `<source_hash>.relon-native-v1` (object cache) and
//! `<source_hash>.relon-ir-v1` (legacy IR cache).
//!
//! Byte format (little-endian, version-prefixed for forward
//! compatibility):
//!
//! ```text
//! magic           : [u8; 4] = b"RLSC"
//! format_version  : u32     = 1
//! body_len        : u32     // body bytes that follow
//! body            : [u8; N] // bincode-encoded SchemaCacheEntry
//! sha256          : [u8; 32]// digest of bytes [0 .. body end]
//! ```

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use relon_eval_api::schema_canonical::Schema;
use relon_parser::TokenRange;

use crate::error::CraneliftError;

/// Magic prefix that tags the file as a relon schema cache.
pub const SCHEMA_CACHE_MAGIC: [u8; 4] = *b"RLSC";

/// Format version. Bump on incompatible layout changes.
pub const SCHEMA_CACHE_VERSION: u32 = 1;

/// Filename suffix for the schema cache. Keep aligned with the
/// object-cache + IR-cache naming so a host's GC sweep over either
/// catches all three.
pub const SCHEMA_CACHE_FILE_SUFFIX: &str = ".relon-schema-v1";

/// Side-table mirror of the bits a `from_cache_dir` constructor needs
/// to rebuild a `CraneliftAotEvaluator` against a dlopen'd ET_DYN.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaCacheEntry {
    /// Canonical schema of the `#main` arguments. The buffer-protocol
    /// trampoline writes user args into this layout before calling
    /// the dlopen'd `run_main`.
    pub main_schema: Schema,
    /// Canonical schema of the `#main` return record.
    pub return_schema: Schema,
    /// Parameter names in declaration order. Surfaced through
    /// `CraneliftAotEvaluator::param_names()`.
    pub param_names: Vec<String>,
    /// Const-data bytes referenced by `Op::ConstString` /
    /// `Op::ConstList*` in the cached object. The trampoline copies
    /// these into the arena prefix before each invocation; the
    /// dlopen'd code dereferences fixed `iconst(I32, offset)` values
    /// against them.
    pub const_data: Vec<u8>,
    /// Number of `__closure_<N>` symbols the loader must `dlsym` from
    /// the ET_DYN. Pairs with the codegen's `closure_table` length.
    pub closure_count: u32,
    /// Entry shape detected at codegen time. Determines whether the
    /// trampoline uses the legacy `(I64...) -> I64` path or the
    /// buffer-protocol `(I32×4, I64) -> I32` path.
    pub entry_shape: SerEntryShape,
    /// Entry arity (number of IR-declared `#main` params; doesn't
    /// count the implicit sandbox-state pointer).
    pub entry_arity: u32,
    /// Source range of the lowered `#main` directive. Carried so the
    /// trampoline can attach diagnostics to traps emitted by the
    /// dlopen'd code.
    pub entry_range: SerTokenRange,
}

/// Serializable mirror of `crate::codegen::EntryShape`. Kept in
/// sync with the codegen-side enum; an incompatible variant change
/// is gated by [`SCHEMA_CACHE_VERSION`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerEntryShape {
    LegacyI64Args = 0,
    BufferProtocol = 1,
}

/// Serializable mirror of [`relon_parser::TokenRange`]. We embed the
/// raw (line, column, offset) pairs so a host that does not depend
/// on `relon-parser`'s internal types can still decode.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct SerTokenRange {
    pub start_line: u32,
    pub start_column: u64,
    pub start_offset: u64,
    pub end_line: u32,
    pub end_column: u64,
    pub end_offset: u64,
}

impl From<TokenRange> for SerTokenRange {
    fn from(r: TokenRange) -> Self {
        Self {
            start_line: r.start.line,
            start_column: r.start.column as u64,
            start_offset: r.start.offset as u64,
            end_line: r.end.line,
            end_column: r.end.column as u64,
            end_offset: r.end.offset as u64,
        }
    }
}

impl From<SerTokenRange> for TokenRange {
    fn from(s: SerTokenRange) -> Self {
        Self {
            start: relon_parser::TokenPosition {
                line: s.start_line,
                column: s.start_column as usize,
                offset: s.start_offset as usize,
            },
            end: relon_parser::TokenPosition {
                line: s.end_line,
                column: s.end_column as usize,
                offset: s.end_offset as usize,
            },
        }
    }
}

/// Encode a [`SchemaCacheEntry`] into the on-disk byte form.
///
/// Uses serde_json for the body because `relon_eval_api::schema_canonical::TypeRepr`
/// is tagged with `#[serde(tag = "kind")]` (internally tagged), which
/// bincode 1.x's lack of `deserialize_any` rejects. JSON keeps the
/// dependency surface narrow and the decode hot path stays under
/// 10 µs for typical entries (< 1 KB).
pub fn serialize(entry: &SchemaCacheEntry) -> Result<Vec<u8>, CraneliftError> {
    let body = serde_json::to_vec(entry)
        .map_err(|e| CraneliftError::Cache(format!("schema cache encode: {e}")))?;
    let body_len = u32::try_from(body.len())
        .map_err(|_| CraneliftError::Cache("schema cache body too large for u32".into()))?;
    let mut out = Vec::with_capacity(body.len() + 16 + 32);
    out.extend_from_slice(&SCHEMA_CACHE_MAGIC);
    out.extend_from_slice(&SCHEMA_CACHE_VERSION.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&body);
    let digest = Sha256::digest(&out);
    out.extend_from_slice(digest.as_slice());
    Ok(out)
}

/// Decode the on-disk byte form back into a [`SchemaCacheEntry`].
pub fn deserialize(bytes: &[u8]) -> Result<SchemaCacheEntry, CraneliftError> {
    if bytes.len() < 4 + 4 + 4 + 32 {
        return Err(CraneliftError::Cache("schema cache too short".into()));
    }
    if bytes[..4] != SCHEMA_CACHE_MAGIC {
        return Err(CraneliftError::Cache("schema cache magic mismatch".into()));
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != SCHEMA_CACHE_VERSION {
        return Err(CraneliftError::Cache(format!(
            "schema cache version mismatch: expected {SCHEMA_CACHE_VERSION}, got {version}"
        )));
    }
    let body_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let body_end = 12 + body_len;
    if bytes.len() < body_end + 32 {
        return Err(CraneliftError::Cache("schema cache truncated".into()));
    }
    let stored_digest = &bytes[body_end..body_end + 32];
    let computed = Sha256::digest(&bytes[..body_end]);
    if computed.as_slice() != stored_digest {
        return Err(CraneliftError::Cache("schema cache sha256 mismatch".into()));
    }
    let entry: SchemaCacheEntry = serde_json::from_slice(&bytes[12..body_end])
        .map_err(|e| CraneliftError::Cache(format!("schema cache decode: {e}")))?;
    Ok(entry)
}

/// Build the canonical schema-cache path next to the object-cache
/// blob. Matching filename stem lets a host's GC catch the pair.
pub fn schema_cache_path_for(
    cache_dir: &std::path::Path,
    source_sha256: [u8; 32],
) -> std::path::PathBuf {
    let mut name = String::with_capacity(64 + SCHEMA_CACHE_FILE_SUFFIX.len());
    for b in source_sha256.iter() {
        use std::fmt::Write as _;
        let _ = write!(&mut name, "{:02x}", b);
    }
    name.push_str(SCHEMA_CACHE_FILE_SUFFIX);
    cache_dir.join(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_eval_api::schema_canonical::{Field, TypeRepr};

    fn sample_entry() -> SchemaCacheEntry {
        let main = Schema {
            name: "MainArgs".into(),
            generics: Vec::new(),
            fields: vec![Field {
                name: "x".into(),
                ty: TypeRepr::Int,
                default: None,
            }],
        };
        let ret = Schema {
            name: "MainReturn".into(),
            generics: Vec::new(),
            fields: vec![Field {
                name: "value".into(),
                ty: TypeRepr::Int,
                default: None,
            }],
        };
        SchemaCacheEntry {
            main_schema: main,
            return_schema: ret,
            param_names: vec!["x".into()],
            const_data: vec![],
            closure_count: 0,
            entry_shape: SerEntryShape::BufferProtocol,
            entry_arity: 1,
            entry_range: SerTokenRange::default(),
        }
    }

    #[test]
    fn schema_cache_round_trip() {
        let entry = sample_entry();
        let bytes = serialize(&entry).expect("serialize");
        let back = deserialize(&bytes).expect("deserialize");
        assert_eq!(back.main_schema.name, entry.main_schema.name);
        assert_eq!(back.return_schema.name, entry.return_schema.name);
        assert_eq!(back.entry_arity, entry.entry_arity);
        assert_eq!(back.entry_shape as u8, entry.entry_shape as u8);
    }

    #[test]
    fn schema_cache_rejects_magic_corruption() {
        let mut bytes = serialize(&sample_entry()).unwrap();
        bytes[0] = b'X';
        let err = deserialize(&bytes).expect_err("magic check");
        assert!(format!("{err}").contains("magic"));
    }

    #[test]
    fn schema_cache_rejects_digest_corruption() {
        let mut bytes = serialize(&sample_entry()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let err = deserialize(&bytes).expect_err("digest");
        assert!(format!("{err}").contains("sha256"));
    }
}
