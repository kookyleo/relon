//! `relon.srcmap` custom section — Phase 1.gamma emit + parse.
//!
//! Binary layout matches `docs/internal/wasm-srcmap-section-v1-2026-05-16.md`:
//!
//! ```text
//! payload:
//!   magic           = b"RLNS"                 (4 bytes)
//!   format_version  = u8 = 1                  (1 byte)
//!   flags           = u8 = 0                  (1 byte, reserved for compression)
//!   file_count      : varuint32
//!   file_table      : [String; file_count]    (each: varuint32 len + utf-8)
//!   entry_count     : varuint32
//!   entries         : [Entry; entry_count]    (sorted by pc ascending)
//!
//! Entry:
//!   pc_delta        : varuint32  (first entry = absolute pc, rest = delta)
//!   file_idx        : varuint32
//!   line            : varuint32  (1-based)
//!   col             : varuint32  (1-based)
//!   range_len       : varuint32  (source char count)
//! ```
//!
//! The custom section is emitted **after** the code section so the `pc`
//! values are stable module-absolute byte offsets that survive wasm
//! validation tools (`wasm-validate`, `wasm-tools`) and runtime engines
//! (wasmtime ignores unknown custom sections at instantiation).

use thiserror::Error;

/// The `relon.srcmap` section name used when emitting the custom section.
/// Hoisted to a constant so the runtime decoder, the emitter, and the
/// integration tests all reference the same string.
pub const SECTION_NAME: &str = "relon.srcmap";

/// 4-byte magic constant identifying a valid Relon srcmap payload.
/// Host SDKs that don't recognise the magic must skip the section
/// (wasm spec permits unknown custom sections).
pub const MAGIC: [u8; 4] = *b"RLNS";

/// Current format version. Incremented when the binary layout changes
/// in a backwards-incompatible way; minor extensions reuse this slot
/// and rely on `flags` for opt-in additions.
pub const FORMAT_VERSION: u8 = 1;

/// One source position entry.
///
/// `pc` here is the **module-absolute byte offset** of the wasm
/// instruction whose source location this entry records. Delta
/// encoding only kicks in at serialise time — in-memory entries
/// always carry the absolute offset for easier sort / lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Module-absolute byte offset of the instruction.
    pub pc: u32,
    /// Index into [`SrcMap::files`].
    pub file_idx: u32,
    /// 1-based source line.
    pub line: u32,
    /// 1-based source column.
    pub col: u32,
    /// Source range length, in characters (not bytes).
    pub range_len: u32,
}

/// In-memory representation of a parsed / about-to-be-encoded srcmap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SrcMap {
    /// Relative source file paths, indexed by [`Entry::file_idx`].
    pub files: Vec<String>,
    /// Entries sorted by ascending pc. Callers building a `SrcMap`
    /// must guarantee this invariant; [`encode_to_bytes`] does not
    /// re-sort (the delta encoding would silently produce garbage
    /// for an out-of-order stream).
    pub entries: Vec<Entry>,
}

impl SrcMap {
    /// Construct an empty srcmap. Equivalent to `Default::default()`,
    /// kept as a named ctor so call sites read clearer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lookup the source entry covering `pc` using binary search.
    /// Returns the entry whose `pc` is the largest value `<= pc`,
    /// or `None` if `pc` is before the first entry.
    ///
    /// Caller-side ordering invariant (entries sorted by ascending pc)
    /// is assumed; violating it produces undefined-but-bounded answers
    /// (always a valid `Entry` reference, just possibly the wrong one).
    pub fn lookup(&self, pc: u32) -> Option<&Entry> {
        if self.entries.is_empty() {
            return None;
        }
        // Partition into "<= pc" prefix and "> pc" suffix; the answer
        // is the last element of the prefix.
        match self.entries.binary_search_by_key(&pc, |e| e.pc) {
            Ok(idx) => Some(&self.entries[idx]),
            Err(0) => None,
            Err(idx) => Some(&self.entries[idx - 1]),
        }
    }
}

/// Reasons srcmap parse / emit can fail.
///
/// Emit-side variants are absent for now — encoding from a well-formed
/// [`SrcMap`] cannot fail (the only invariant is "entries sorted by pc"
/// and we don't validate it). All variants are decoder-side.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SrcMapError {
    /// Payload shorter than the fixed-size header (`MAGIC + version + flags`).
    /// Indicates a stripped or truncated section.
    #[error("srcmap payload truncated at offset {at}: expected {need} more bytes")]
    Truncated { at: usize, need: usize },
    /// Magic prefix did not match `b"RLNS"`. The decoder treats this as
    /// "not our section" and the host SDK is expected to fall back to
    /// raw wasm trap reporting.
    #[error("srcmap magic mismatch: expected RLNS, got {got:?}")]
    BadMagic { got: [u8; 4] },
    /// Format version is newer than this decoder knows about. Host SDK
    /// should refuse-to-load or strip the section; never silently parse.
    #[error("srcmap format version {got} is newer than supported version {supported}")]
    FutureFormat { got: u8, supported: u8 },
    /// `flags` byte has bits this version doesn't recognise. v1 spec
    /// reserves all bits; any non-zero value here means producer used
    /// a feature this decoder doesn't model.
    #[error("srcmap flags byte {flags:#04x} has unrecognised bits set")]
    UnknownFlags { flags: u8 },
    /// LEB128 varuint decode hit EOF or oversized value (>32 bits).
    /// Always indicates a corrupted payload.
    #[error("invalid varuint32 at offset {at}: {reason}")]
    InvalidVarint { at: usize, reason: &'static str },
    /// File table or entry references a UTF-8 sequence that doesn't decode.
    #[error("file path at offset {at} is not valid UTF-8")]
    BadUtf8 { at: usize },
    /// pc delta overflows `u32` when accumulated against the prior pc.
    /// Encoded payloads are validated against this on the way in so the
    /// in-memory `Vec<Entry>` always has monotonically increasing pcs.
    #[error("pc delta at entry {index} overflows u32 (prev_pc={prev_pc}, delta={delta})")]
    PcOverflow {
        index: usize,
        prev_pc: u32,
        delta: u32,
    },
    /// `file_idx` on an entry exceeds the declared `files.len()`. This
    /// is decoder-side because we want to fail fast at parse time
    /// rather than at lookup time.
    #[error("entry {index} references file_idx {file_idx} but file table has only {count}")]
    FileIdxOutOfRange {
        index: usize,
        file_idx: u32,
        count: u32,
    },
}

// ---------------------------------------------------------------------------
// LEB128 unsigned varint helpers (32-bit). The wasm spec uses the same
// encoding, so the byte layout here is interoperable with any wasm tool
// that exposes raw custom-section bytes.
// ---------------------------------------------------------------------------

/// Append a LEB128-encoded unsigned 32-bit value to `out`.
///
/// Standard 7-bit-per-byte encoding with continuation bit. A `u32::MAX`
/// expands to 5 bytes; small numbers occupy a single byte. No alloc
/// happens beyond `out`'s growth.
pub fn encode_varu32(value: u32, out: &mut Vec<u8>) {
    let mut v = value;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        } else {
            out.push(byte | 0x80);
        }
    }
}

/// Read a LEB128-encoded unsigned 32-bit value from `bytes[*cursor..]`,
/// advancing `*cursor` past the consumed bytes.
///
/// Returns [`SrcMapError::InvalidVarint`] on EOF, missing continuation
/// terminator, or payload exceeding 32 bits.
pub fn decode_varu32(bytes: &[u8], cursor: &mut usize) -> Result<u32, SrcMapError> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    // LEB128 unsigned 32 fits in at most ceil(32/7) = 5 bytes.
    for byte_idx in 0..5 {
        let at = *cursor;
        let byte = *bytes.get(at).ok_or(SrcMapError::InvalidVarint {
            at,
            reason: "unexpected end of input",
        })?;
        *cursor += 1;
        let low7 = (byte & 0x7f) as u32;
        // Last byte (5th) must not have any bits beyond u32's top set,
        // otherwise the encoded value overflows.
        if byte_idx == 4 && (byte & 0x70) != 0 {
            return Err(SrcMapError::InvalidVarint {
                at,
                reason: "value exceeds u32 range",
            });
        }
        result |= low7.checked_shl(shift).ok_or(SrcMapError::InvalidVarint {
            at,
            reason: "shift overflow",
        })?;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
    // Fell out of the loop without finding the terminator byte.
    Err(SrcMapError::InvalidVarint {
        at: *cursor,
        reason: "missing terminator byte",
    })
}

// ---------------------------------------------------------------------------
// Encode / decode
// ---------------------------------------------------------------------------

/// Encode a [`SrcMap`] into the raw custom-section payload (without the
/// outer wasm section header — that's added when the caller wraps the
/// bytes in a `wasm_encoder::CustomSection`).
///
/// The encoder assumes `srcmap.entries` is already sorted by ascending
/// pc. Out-of-order entries are encoded verbatim — the resulting blob
/// will decode without error but lookup semantics are then undefined.
pub fn encode_to_bytes(srcmap: &SrcMap) -> Vec<u8> {
    // Rough sizing: header (6 bytes) + file table + entries. Each
    // entry is ~5 fields * ~2 bytes average = 10 bytes. Pre-allocate
    // so most realistic modules don't re-grow the vec.
    let cap =
        6 + srcmap.files.iter().map(|s| s.len() + 2).sum::<usize>() + srcmap.entries.len() * 12;
    let mut out = Vec::with_capacity(cap);

    // Header.
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    out.push(0u8); // flags: v1 always 0

    // File table.
    encode_varu32(srcmap.files.len() as u32, &mut out);
    for path in &srcmap.files {
        let bytes = path.as_bytes();
        encode_varu32(bytes.len() as u32, &mut out);
        out.extend_from_slice(bytes);
    }

    // Entries with pc delta encoding.
    encode_varu32(srcmap.entries.len() as u32, &mut out);
    let mut prev_pc: u32 = 0;
    for entry in &srcmap.entries {
        // First entry: pc is absolute (i.e. delta from 0).
        // Subsequent: delta from previous entry's pc. The lookup-side
        // invariant (entries sorted ascending) means pc - prev_pc never
        // underflows when callers respect ordering. If they don't,
        // `wrapping_sub` produces a large positive delta that decodes
        // back to a wrong pc — same garbage in, garbage out contract.
        let delta = entry.pc.wrapping_sub(prev_pc);
        encode_varu32(delta, &mut out);
        encode_varu32(entry.file_idx, &mut out);
        encode_varu32(entry.line, &mut out);
        encode_varu32(entry.col, &mut out);
        encode_varu32(entry.range_len, &mut out);
        prev_pc = entry.pc;
    }

    out
}

/// Decode the raw `relon.srcmap` payload (i.e. the bytes inside the
/// custom section, after the wasm section header has been stripped) into
/// a [`SrcMap`].
///
/// The decoder enforces:
/// * 4-byte magic prefix `b"RLNS"`.
/// * Format version equals [`FORMAT_VERSION`].
/// * Flags byte is `0` (v1 reserves all bits).
/// * Each file path decodes as valid UTF-8.
/// * pc deltas don't overflow `u32` when accumulated.
/// * `file_idx` for every entry is within `files.len()`.
pub fn decode_from_bytes(bytes: &[u8]) -> Result<SrcMap, SrcMapError> {
    // Header: magic (4) + version (1) + flags (1) = 6 bytes minimum.
    if bytes.len() < 6 {
        return Err(SrcMapError::Truncated {
            at: bytes.len(),
            need: 6 - bytes.len(),
        });
    }

    let magic: [u8; 4] = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if magic != MAGIC {
        return Err(SrcMapError::BadMagic { got: magic });
    }

    let version = bytes[4];
    if version > FORMAT_VERSION {
        return Err(SrcMapError::FutureFormat {
            got: version,
            supported: FORMAT_VERSION,
        });
    }
    // Past tense (version < FORMAT_VERSION) would also be a `FutureFormat`-style
    // mismatch for a host built against an older spec; v1 is the first version
    // so we don't try to model that yet.

    let flags = bytes[5];
    if flags != 0 {
        return Err(SrcMapError::UnknownFlags { flags });
    }

    let mut cursor: usize = 6;

    // File table.
    let file_count = decode_varu32(bytes, &mut cursor)?;
    let mut files: Vec<String> = Vec::with_capacity(file_count as usize);
    for _ in 0..file_count {
        let len = decode_varu32(bytes, &mut cursor)? as usize;
        let end = cursor.checked_add(len).ok_or(SrcMapError::InvalidVarint {
            at: cursor,
            reason: "file path length overflows usize",
        })?;
        if end > bytes.len() {
            return Err(SrcMapError::Truncated {
                at: cursor,
                need: end - bytes.len(),
            });
        }
        let slice = &bytes[cursor..end];
        let s = std::str::from_utf8(slice)
            .map_err(|_| SrcMapError::BadUtf8 { at: cursor })?
            .to_owned();
        files.push(s);
        cursor = end;
    }

    // Entries.
    let entry_count = decode_varu32(bytes, &mut cursor)?;
    let mut entries: Vec<Entry> = Vec::with_capacity(entry_count as usize);
    let mut prev_pc: u32 = 0;
    for index in 0..entry_count as usize {
        let delta = decode_varu32(bytes, &mut cursor)?;
        let pc = prev_pc.checked_add(delta).ok_or(SrcMapError::PcOverflow {
            index,
            prev_pc,
            delta,
        })?;
        let file_idx = decode_varu32(bytes, &mut cursor)?;
        let line = decode_varu32(bytes, &mut cursor)?;
        let col = decode_varu32(bytes, &mut cursor)?;
        let range_len = decode_varu32(bytes, &mut cursor)?;
        if file_idx >= file_count {
            return Err(SrcMapError::FileIdxOutOfRange {
                index,
                file_idx,
                count: file_count,
            });
        }
        entries.push(Entry {
            pc,
            file_idx,
            line,
            col,
            range_len,
        });
        prev_pc = pc;
    }

    Ok(SrcMap { files, entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // varuint32 round-trip
    // ---------------------------------------------------------------

    #[test]
    fn varuint32_roundtrip_small_values() {
        for v in [0u32, 1, 63, 127, 128, 16_383, 16_384, 1_000_000, u32::MAX] {
            let mut buf = Vec::new();
            encode_varu32(v, &mut buf);
            let mut cursor = 0;
            let decoded = decode_varu32(&buf, &mut cursor).expect("decode");
            assert_eq!(decoded, v, "varint roundtrip for {v}");
            assert_eq!(cursor, buf.len(), "cursor must consume entire buffer");
        }
    }

    #[test]
    fn varuint32_missing_terminator() {
        // All bytes have continuation bit set, never terminating.
        let bad: Vec<u8> = vec![0x80, 0x80, 0x80, 0x80, 0x80];
        let mut cursor = 0;
        let err = decode_varu32(&bad, &mut cursor).expect_err("must reject");
        assert!(matches!(err, SrcMapError::InvalidVarint { .. }));
    }

    #[test]
    fn varuint32_eof_mid_value() {
        let bad: Vec<u8> = vec![0x80, 0x80];
        let mut cursor = 0;
        let err = decode_varu32(&bad, &mut cursor).expect_err("must reject");
        assert!(matches!(err, SrcMapError::InvalidVarint { .. }));
    }

    // ---------------------------------------------------------------
    // SrcMap encode / decode roundtrip
    // ---------------------------------------------------------------

    #[test]
    fn empty_srcmap_roundtrip() {
        let srcmap = SrcMap::new();
        let bytes = encode_to_bytes(&srcmap);
        let decoded = decode_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, srcmap);
    }

    #[test]
    fn single_entry_roundtrip() {
        let srcmap = SrcMap {
            files: vec!["main.relon".to_string()],
            entries: vec![Entry {
                pc: 42,
                file_idx: 0,
                line: 3,
                col: 5,
                range_len: 7,
            }],
        };
        let bytes = encode_to_bytes(&srcmap);
        let decoded = decode_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, srcmap);
    }

    #[test]
    fn multi_entry_delta_roundtrip() {
        // Three entries with non-monotonic spacing exercises the delta
        // encoder: the second delta is small, the third is large.
        let srcmap = SrcMap {
            files: vec!["foo.relon".into(), "bar.relon".into()],
            entries: vec![
                Entry {
                    pc: 10,
                    file_idx: 0,
                    line: 1,
                    col: 1,
                    range_len: 3,
                },
                Entry {
                    pc: 12,
                    file_idx: 1,
                    line: 2,
                    col: 5,
                    range_len: 4,
                },
                Entry {
                    pc: 250,
                    file_idx: 0,
                    line: 7,
                    col: 1,
                    range_len: 12,
                },
            ],
        };
        let bytes = encode_to_bytes(&srcmap);
        let decoded = decode_from_bytes(&bytes).expect("decode");
        assert_eq!(decoded, srcmap);

        // Sanity: byte layout starts with the magic so a third-party
        // tool can sniff the section type without parsing further.
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(bytes[4], FORMAT_VERSION);
        assert_eq!(bytes[5], 0);
    }

    // ---------------------------------------------------------------
    // Decoder failure surface
    // ---------------------------------------------------------------

    #[test]
    fn header_too_short() {
        let err = decode_from_bytes(&[]).expect_err("must reject");
        assert!(matches!(err, SrcMapError::Truncated { .. }));
        let err = decode_from_bytes(b"RLN").expect_err("must reject");
        assert!(matches!(err, SrcMapError::Truncated { .. }));
    }

    #[test]
    fn bad_magic() {
        let mut bytes = encode_to_bytes(&SrcMap::new());
        bytes[0] = b'X';
        let err = decode_from_bytes(&bytes).expect_err("must reject");
        assert!(
            matches!(err, SrcMapError::BadMagic { got } if got == [b'X', b'L', b'N', b'S']),
            "got {err:?}",
        );
    }

    #[test]
    fn future_format_version() {
        let mut bytes = encode_to_bytes(&SrcMap::new());
        bytes[4] = 99;
        let err = decode_from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(
            err,
            SrcMapError::FutureFormat {
                got: 99,
                supported: 1
            }
        ));
    }

    #[test]
    fn unknown_flags() {
        let mut bytes = encode_to_bytes(&SrcMap::new());
        bytes[5] = 0b0000_0010; // bit 1: dhat-trace embedded (v2+) per spec
        let err = decode_from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(err, SrcMapError::UnknownFlags { .. }));
    }

    #[test]
    fn file_idx_out_of_range_rejected() {
        // Hand-craft a payload referencing file_idx 1 with an empty
        // file table.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.push(FORMAT_VERSION);
        bytes.push(0);
        encode_varu32(0, &mut bytes); // file_count = 0
        encode_varu32(1, &mut bytes); // entry_count = 1
        encode_varu32(5, &mut bytes); // pc delta = 5
        encode_varu32(1, &mut bytes); // file_idx = 1 (BAD)
        encode_varu32(1, &mut bytes); // line
        encode_varu32(1, &mut bytes); // col
        encode_varu32(1, &mut bytes); // range_len
        let err = decode_from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(err, SrcMapError::FileIdxOutOfRange { .. }));
    }

    // ---------------------------------------------------------------
    // Lookup
    // ---------------------------------------------------------------

    #[test]
    fn lookup_returns_largest_pc_not_exceeding_query() {
        let srcmap = SrcMap {
            files: vec!["main.relon".into()],
            entries: vec![
                Entry {
                    pc: 10,
                    file_idx: 0,
                    line: 1,
                    col: 1,
                    range_len: 1,
                },
                Entry {
                    pc: 20,
                    file_idx: 0,
                    line: 2,
                    col: 1,
                    range_len: 1,
                },
                Entry {
                    pc: 30,
                    file_idx: 0,
                    line: 3,
                    col: 1,
                    range_len: 1,
                },
            ],
        };
        assert_eq!(srcmap.lookup(9), None, "before first entry");
        assert_eq!(srcmap.lookup(10).map(|e| e.line), Some(1));
        assert_eq!(srcmap.lookup(15).map(|e| e.line), Some(1));
        assert_eq!(srcmap.lookup(20).map(|e| e.line), Some(2));
        assert_eq!(
            srcmap.lookup(100).map(|e| e.line),
            Some(3),
            "past last entry sticks to last"
        );
    }
}
