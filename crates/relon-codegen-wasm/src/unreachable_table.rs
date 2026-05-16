//! Phase 7: `relon.uctab` custom section — per-`unreachable` metadata
//! recorded by codegen so the runtime trap translator can recover the
//! semantic intent of the guard that fired (capability denied vs.
//! buffer-size guard vs. tail-cursor overflow) without disassembling
//! the surrounding wasm bytes.
//!
//! Why a separate section: keeping the srcmap section clean. The
//! srcmap maps every emitted instruction (about 6 per guard) to a
//! source range; this table maps just the *trap-emitting* `unreachable`
//! pcs (one per guard) to a small semantic tag. The two are read
//! together at trap translation time but evolve independently — a
//! future phase that gains a new guard shape extends `UnreachableKind`
//! without re-shaping the srcmap entries.
//!
//! Binary layout (mirrors the `relon.srcmap` style — varuint32-heavy,
//! little-endian-implicit, delta-coded pcs):
//!
//! ```text
//! payload:
//!   magic            = b"RLUC"                 (4 bytes)
//!   format_version   = u8 = 1                  (1 byte)
//!   flags            = u8 = 0                  (1 byte, reserved)
//!   entry_count      : varuint32
//!   entries          : [Entry; entry_count]    (sorted by pc ascending)
//!
//! Entry:
//!   pc_delta         : varuint32   (first = absolute, rest = delta)
//!   kind_tag         : varuint32   (0..=3 — see `UnreachableKind`)
//!   payload          : varuint32   (per-kind: cap_bit, needed bytes, or
//!                                   index into the kind-string pool)
//! ```
//!
//! For `ValueTooLarge` the payload is an index into a hard-coded
//! ASCII tag table (`String`, `ListInt`, `Record`); this keeps the
//! decoder allocation-free while still letting the trap translator
//! surface a meaningful `kind: &'static str`.

use crate::srcmap::{decode_varu32, encode_varu32, SrcMapError};
use thiserror::Error;

/// Section name used when emitting the `relon.uctab` custom section.
pub const SECTION_NAME: &str = "relon.uctab";

/// 4-byte magic prefix. `RLUC` = Relon UnreachaCable. Any blob whose
/// first four bytes disagree is treated as a stripped / corrupted
/// section and produces [`UnreachableTableError::BadMagic`].
pub const MAGIC: [u8; 4] = *b"RLUC";

/// Current format version. Phase 7 emits / consumes v1; a future
/// extension that adds new [`UnreachableKind`] variants bumps this.
pub const FORMAT_VERSION: u8 = 1;

// `kind_tag` discriminants. Kept stable across versions — the encoder
// must never reuse a tag that already shipped in a release.
const TAG_CAPABILITY_DENIED: u32 = 0;
const TAG_OUT_BUF_TOO_SMALL: u32 = 1;
const TAG_IN_BUF_TOO_SMALL: u32 = 2;
const TAG_VALUE_TOO_LARGE: u32 = 3;

// Indexes into the static "kind" tag table used by [`UnreachableKind::ValueTooLarge`].
// New tags append; the existing indices are part of the on-disk format.
const VALUE_KIND_TAGS: &[&str] = &["String", "ListInt", "Record"];

/// Semantic intent of a wasm `unreachable` instruction emitted by
/// the Relon codegen. Each enumerated variant corresponds to exactly
/// one guard shape; an `unreachable` outside this table (e.g. emitted
/// by a future phase before the table is extended) decodes back into
/// nothing — the trap translator falls through to
/// `RuntimeError::WasmTrapUnclassified`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreachableKind {
    /// `check_cap` prologue tripped: the `relon_caps_avail` bitmap
    /// lacks the bit required by a `#native fn` invocation.
    CapabilityDenied {
        /// Bit index (0-based) tested by the prologue. Mirrors the
        /// `cap_bit` field on the wasm module's host-fn entry.
        cap_bit: u32,
    },
    /// Entry-function `out_cap` guard tripped because `out_cap` is
    /// less than the return schema's fixed-area root size.
    OutBufTooSmall {
        /// Minimum bytes the guard required.
        needed: u32,
    },
    /// Entry-function `in_len` guard tripped because `in_len` is
    /// less than the `#main` param schema's fixed-area root size.
    InBufTooSmall {
        /// Minimum bytes the guard required.
        needed: u32,
    },
    /// A tail-record bounds check (`StoreField` of `String` /
    /// `List<Int>`, or `AllocSubRecord`) overran the caller's
    /// `out_cap`. The `kind` tag tells the trap translator which
    /// shape ran over so the surfaced `RuntimeError` carries a
    /// meaningful descriptor.
    ValueTooLarge {
        /// Stable `'static` tag from [`VALUE_KIND_TAGS`].
        kind: &'static str,
    },
}

impl UnreachableKind {
    /// Encode the tag-and-payload pair this kind owns. Returns
    /// `(kind_tag, payload)` matching the binary layout described
    /// at the module level.
    fn encode_payload(&self) -> (u32, u32) {
        match self {
            UnreachableKind::CapabilityDenied { cap_bit } => (TAG_CAPABILITY_DENIED, *cap_bit),
            UnreachableKind::OutBufTooSmall { needed } => (TAG_OUT_BUF_TOO_SMALL, *needed),
            UnreachableKind::InBufTooSmall { needed } => (TAG_IN_BUF_TOO_SMALL, *needed),
            UnreachableKind::ValueTooLarge { kind } => {
                let idx = VALUE_KIND_TAGS
                    .iter()
                    .position(|t| *t == *kind)
                    // Unknown kind strings are a codegen bug — we
                    // shouldn't reach here from valid emit sites.
                    // Encode as the placeholder `0` so the decoder
                    // still produces a usable `RuntimeError`; the
                    // surfaced kind label will be the first entry
                    // ("String") which is at least non-empty.
                    .unwrap_or(0) as u32;
                (TAG_VALUE_TOO_LARGE, idx)
            }
        }
    }
}

/// In-memory representation of a parsed / about-to-be-encoded
/// `relon.uctab` payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnreachableTable {
    /// Entries sorted by ascending pc. Callers that mutate this
    /// directly must re-sort before emit; the encoder doesn't.
    pub entries: Vec<UnreachableEntry>,
}

/// One row in the `relon.uctab` table. Same `pc` semantics as
/// [`crate::srcmap::Entry`] — module-absolute byte offset of the
/// trapping `unreachable` instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnreachableEntry {
    /// Module-absolute byte offset of the `unreachable` instruction.
    pub pc: u32,
    /// Semantic intent of the guard.
    pub kind: UnreachableKind,
}

impl UnreachableTable {
    /// Construct an empty table. Equivalent to `Default::default()`,
    /// kept as a named ctor so call sites read clearer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lookup the [`UnreachableKind`] for an exact pc match. Returns
    /// `None` when no codegen-emitted `unreachable` lives at this
    /// offset — the trap translator then surfaces a
    /// `WasmTrapUnclassified` so the caller still gets something to
    /// log.
    ///
    /// The exact-match contract matters: the wasm runtime always
    /// reports the trapping instruction's pc, so we don't need the
    /// "largest pc ≤ query" partition the srcmap uses. A miss is
    /// always a real "this `unreachable` isn't from our codegen"
    /// signal.
    pub fn lookup(&self, pc: u32) -> Option<UnreachableKind> {
        self.entries
            .binary_search_by_key(&pc, |e| e.pc)
            .ok()
            .map(|idx| self.entries[idx].kind)
    }
}

/// Decode error surface for the `relon.uctab` section. Mirrors
/// [`SrcMapError`] in spirit but uses a section-specific name space.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UnreachableTableError {
    /// Payload shorter than the fixed-size header (`MAGIC + version + flags`).
    #[error("uctab payload truncated at offset {at}: expected {need} more bytes")]
    Truncated { at: usize, need: usize },
    /// Magic prefix did not match `b"RLUC"`. The decoder treats this
    /// as "not our section".
    #[error("uctab magic mismatch: expected RLUC, got {got:?}")]
    BadMagic { got: [u8; 4] },
    /// Format version newer than this decoder knows about.
    #[error("uctab format version {got} is newer than supported version {supported}")]
    FutureFormat { got: u8, supported: u8 },
    /// `flags` byte has bits this version doesn't recognise.
    #[error("uctab flags byte {flags:#04x} has unrecognised bits set")]
    UnknownFlags { flags: u8 },
    /// LEB128 varuint decode hit EOF or oversized value (>32 bits).
    #[error("invalid varuint32 at offset {at}: {reason}")]
    InvalidVarint { at: usize, reason: &'static str },
    /// pc delta overflows `u32` when accumulated against the prior pc.
    #[error("pc delta at entry {index} overflows u32 (prev_pc={prev_pc}, delta={delta})")]
    PcOverflow {
        index: usize,
        prev_pc: u32,
        delta: u32,
    },
    /// `kind_tag` discriminant is outside the known range. Indicates
    /// a forward-compat extension this decoder doesn't model.
    #[error("uctab entry {index} has unknown kind_tag {tag}")]
    UnknownKindTag { index: usize, tag: u32 },
    /// `ValueTooLarge` payload index falls outside the known kind
    /// table. Same forward-compat shape as [`Self::UnknownKindTag`].
    #[error("uctab entry {index} value-kind index {idx} exceeds table size {len}")]
    ValueKindOutOfRange { index: usize, idx: u32, len: u32 },
}

// ---------------------------------------------------------------------------
// Internal: forward `SrcMapError::InvalidVarint` shapes into our error.
// ---------------------------------------------------------------------------

fn map_varint(err: SrcMapError) -> UnreachableTableError {
    match err {
        SrcMapError::InvalidVarint { at, reason } => {
            UnreachableTableError::InvalidVarint { at, reason }
        }
        // The other variants are decoder-side guarantees that don't
        // apply to our payload shape; fall back to a generic varint
        // error so the surface stays narrow.
        _ => UnreachableTableError::InvalidVarint {
            at: 0,
            reason: "unexpected upstream srcmap error",
        },
    }
}

// ---------------------------------------------------------------------------
// Encode / decode
// ---------------------------------------------------------------------------

/// Encode an [`UnreachableTable`] into the raw custom-section payload.
/// Assumes `table.entries` is already sorted by ascending pc — out-of-
/// order entries encode verbatim, decode without error, but produce
/// undefined lookup answers (mirrors the srcmap encoder's stance).
pub fn encode_to_bytes(table: &UnreachableTable) -> Vec<u8> {
    let mut out = Vec::with_capacity(6 + table.entries.len() * 6);
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    out.push(0u8); // flags reserved
    encode_varu32(table.entries.len() as u32, &mut out);
    let mut prev_pc: u32 = 0;
    for entry in &table.entries {
        let delta = entry.pc.wrapping_sub(prev_pc);
        encode_varu32(delta, &mut out);
        let (tag, payload) = entry.kind.encode_payload();
        encode_varu32(tag, &mut out);
        encode_varu32(payload, &mut out);
        prev_pc = entry.pc;
    }
    out
}

/// Decode the raw `relon.uctab` payload into an [`UnreachableTable`].
/// The decoder enforces magic / version / flags shape and rejects any
/// out-of-range kind tag or value-kind index so a stale codegen
/// payload can't smuggle through unrecognised metadata.
pub fn decode_from_bytes(bytes: &[u8]) -> Result<UnreachableTable, UnreachableTableError> {
    if bytes.len() < 6 {
        return Err(UnreachableTableError::Truncated {
            at: bytes.len(),
            need: 6 - bytes.len(),
        });
    }

    let magic: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if magic != MAGIC {
        return Err(UnreachableTableError::BadMagic { got: magic });
    }
    let version = bytes[4];
    if version > FORMAT_VERSION {
        return Err(UnreachableTableError::FutureFormat {
            got: version,
            supported: FORMAT_VERSION,
        });
    }
    let flags = bytes[5];
    if flags != 0 {
        return Err(UnreachableTableError::UnknownFlags { flags });
    }

    let mut cursor: usize = 6;
    let entry_count = decode_varu32(bytes, &mut cursor).map_err(map_varint)?;
    let mut entries: Vec<UnreachableEntry> = Vec::with_capacity(entry_count as usize);
    let mut prev_pc: u32 = 0;
    for index in 0..entry_count as usize {
        let delta = decode_varu32(bytes, &mut cursor).map_err(map_varint)?;
        let pc = prev_pc
            .checked_add(delta)
            .ok_or(UnreachableTableError::PcOverflow {
                index,
                prev_pc,
                delta,
            })?;
        let tag = decode_varu32(bytes, &mut cursor).map_err(map_varint)?;
        let payload = decode_varu32(bytes, &mut cursor).map_err(map_varint)?;
        let kind = match tag {
            TAG_CAPABILITY_DENIED => UnreachableKind::CapabilityDenied { cap_bit: payload },
            TAG_OUT_BUF_TOO_SMALL => UnreachableKind::OutBufTooSmall { needed: payload },
            TAG_IN_BUF_TOO_SMALL => UnreachableKind::InBufTooSmall { needed: payload },
            TAG_VALUE_TOO_LARGE => {
                let idx = payload as usize;
                let kind_str =
                    VALUE_KIND_TAGS
                        .get(idx)
                        .ok_or(UnreachableTableError::ValueKindOutOfRange {
                            index,
                            idx: payload,
                            len: VALUE_KIND_TAGS.len() as u32,
                        })?;
                UnreachableKind::ValueTooLarge { kind: kind_str }
            }
            other => return Err(UnreachableTableError::UnknownKindTag { index, tag: other }),
        };
        entries.push(UnreachableEntry { pc, kind });
        prev_pc = pc;
    }

    Ok(UnreachableTable { entries })
}

/// Stable accessor for the value-kind tag pool. Codegen helpers that
/// construct [`UnreachableKind::ValueTooLarge`] feed one of these
/// strings so the resulting entry round-trips through encode / decode.
pub fn value_kind_tag(idx: usize) -> Option<&'static str> {
    VALUE_KIND_TAGS.get(idx).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_roundtrip() {
        let table = UnreachableTable::new();
        let bytes = encode_to_bytes(&table);
        let decoded = decode_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, table);
    }

    #[test]
    fn multi_entry_roundtrip() {
        let table = UnreachableTable {
            entries: vec![
                UnreachableEntry {
                    pc: 17,
                    kind: UnreachableKind::OutBufTooSmall { needed: 8 },
                },
                UnreachableEntry {
                    pc: 32,
                    kind: UnreachableKind::CapabilityDenied { cap_bit: 0 },
                },
                UnreachableEntry {
                    pc: 200,
                    kind: UnreachableKind::ValueTooLarge { kind: "ListInt" },
                },
            ],
        };
        let bytes = encode_to_bytes(&table);
        let decoded = decode_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, table);
        assert_eq!(
            decoded.lookup(32),
            Some(UnreachableKind::CapabilityDenied { cap_bit: 0 })
        );
        assert_eq!(decoded.lookup(33), None, "exact-match contract");
    }

    #[test]
    fn header_too_short() {
        let err = decode_from_bytes(&[]).expect_err("must reject");
        assert!(matches!(err, UnreachableTableError::Truncated { .. }));
    }

    #[test]
    fn bad_magic() {
        let mut bytes = encode_to_bytes(&UnreachableTable::new());
        bytes[0] = b'X';
        let err = decode_from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(err, UnreachableTableError::BadMagic { .. }));
    }

    #[test]
    fn unknown_tag_rejected() {
        // Hand-craft a payload with kind_tag = 99.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(FORMAT_VERSION);
        bytes.push(0);
        encode_varu32(1, &mut bytes); // entry_count
        encode_varu32(5, &mut bytes); // pc delta
        encode_varu32(99, &mut bytes); // unknown tag
        encode_varu32(0, &mut bytes); // payload
        let err = decode_from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(
            err,
            UnreachableTableError::UnknownKindTag { tag: 99, .. }
        ));
    }

    #[test]
    fn value_kind_out_of_range_rejected() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(FORMAT_VERSION);
        bytes.push(0);
        encode_varu32(1, &mut bytes); // entry_count
        encode_varu32(5, &mut bytes); // pc delta
        encode_varu32(TAG_VALUE_TOO_LARGE, &mut bytes);
        encode_varu32(99, &mut bytes); // payload out of range
        let err = decode_from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(
            err,
            UnreachableTableError::ValueKindOutOfRange { idx: 99, .. }
        ));
    }
}
