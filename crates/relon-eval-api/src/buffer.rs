//! Typesafe writer + reader for the host <-> wasm binary handshake.
//!
//! Spec: `docs/internal/wasm-binary-layout-v1-2026-05-16.md`.
//!
//! [`BufferBuilder`] writes scalar fields by name into a pre-allocated
//! byte buffer sized by a [`crate::layout::OffsetTable`]; [`BufferReader`]
//! reads the same fields back. The intermediate `Vec<u8>` is exactly
//! what the host hands the wasm module (`run_main(in_ptr, in_len,
//! out_ptr)`) once Phase 2.b flips the wasm signature; in Phase 2.a
//! the binary path is dormant but the writer/reader infrastructure is
//! already in place so host code can be wired ahead of the codegen
//! flip.
//!
//! Type safety is enforced at runtime by checking the schema's
//! declared type for each field against the writer call (e.g.
//! `write_int` on a `Bool` slot is [`BufferError::TypeMismatch`]).
//! Static enforcement via codegen-generated wrappers is a Phase 3
//! follow-up.

use crate::layout::{FieldKind, OffsetTable};
use crate::schema_canonical::{Field, TypeRepr};
use thiserror::Error;

/// Failure modes when writing / reading typed fields against a buffer.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BufferError {
    /// The requested field name does not appear in the offset table.
    /// Indicates a schema-drift bug or a typo at the call site.
    #[error("unknown field `{name}`")]
    UnknownField {
        /// Field name passed to the writer / reader.
        name: String,
    },
    /// The requested write / read type does not match the field's
    /// declared type. Writing an `i64` to a `Bool` slot would corrupt
    /// the layout for adjacent fields, so this surfaces as a hard
    /// error rather than a silent reinterpret.
    #[error(
        "type mismatch for field `{name}`: schema declares {declared}, caller used {requested}"
    )]
    TypeMismatch {
        /// Field name being accessed.
        name: String,
        /// Type the schema declared for this field.
        declared: &'static str,
        /// Type the caller's writer / reader assumed.
        requested: &'static str,
    },
    /// The buffer is shorter than the offset table's `root_size`.
    /// Always indicates the buffer was truncated mid-flight (e.g. an
    /// `out_buf` the wasm module didn't fully fill).
    #[error("buffer too small: have {have} bytes, layout requires {need}")]
    BufferTooSmall {
        /// Actual byte length of the buffer.
        have: usize,
        /// Required length per the offset table.
        need: usize,
    },
    /// A pointer-indirect payload (String / List<Int>) is larger than
    /// the `u32` length prefix can describe. Phase 2.c caps each
    /// payload at `u32::MAX` bytes / elements; longer values surface
    /// here rather than overflow silently.
    #[error("payload for field `{name}` is too large: {len} exceeds u32::MAX")]
    ValueTooLarge {
        /// Field name carrying the oversized payload.
        name: String,
        /// Requested length (bytes for String, elements for List<Int>).
        len: usize,
    },
    /// A pointer-indirect read tripped over a malformed tail-area
    /// payload — the length prefix points beyond the buffer end, the
    /// pointer is null when the schema expects a value, or the
    /// utf-8 bytes inside a `String` payload are invalid.
    #[error("malformed payload for field `{name}`: {reason}")]
    MalformedPayload {
        /// Field name being read.
        name: String,
        /// Why the payload couldn't be decoded.
        reason: &'static str,
    },
}

/// Type-checked writer over a record buffer with an optional tail
/// area.
///
/// Phase 2.c shape:
///
/// * The fixed area is pre-allocated to `layout.root_size` so every
///   inline scalar slot is well-defined zero bytes per the spec.
/// * String / List<Int> writes append a `[len: u32 LE][payload]`
///   record after the fixed area and back-patch the pointer slot in
///   the fixed area with the tail-record's byte offset (relative to
///   the buffer start — the wasm side adds `in_ptr` to it).
///
/// Lifetime tie-in: the builder borrows the offset table so the same
/// layout description can be reused for the matching reader without
/// reparsing.
#[derive(Debug)]
pub struct BufferBuilder<'a> {
    layout: &'a OffsetTable,
    field_index: Vec<(String, TypeRepr, usize, usize, FieldKind)>,
    bytes: Vec<u8>,
}

impl<'a> BufferBuilder<'a> {
    /// Build a writer for `layout` with the byte buffer pre-zeroed to
    /// `layout.root_size`.
    ///
    /// `fields` carries the schema-level type info the layout pass
    /// already validated; we keep a side index so the writer / reader
    /// can detect a type mismatch without re-walking the schema.
    pub fn new(layout: &'a OffsetTable, fields: &[Field]) -> Self {
        let bytes = vec![0u8; layout.root_size];
        let field_index = layout
            .fields
            .iter()
            .filter_map(|fo| {
                fields
                    .iter()
                    .find(|f| f.name == fo.name)
                    .map(|f| (fo.name.clone(), f.ty.clone(), fo.offset, fo.size, fo.kind))
            })
            .collect();
        Self {
            layout,
            field_index,
            bytes,
        }
    }

    /// Write a 64-bit signed integer to `field_name`.
    pub fn write_int(&mut self, field_name: &str, value: i64) -> Result<(), BufferError> {
        let (offset, _, _) = self.locate(field_name, &TypeRepr::Int, "Int")?;
        self.bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Write a 64-bit float to `field_name`.
    pub fn write_float(&mut self, field_name: &str, value: f64) -> Result<(), BufferError> {
        let (offset, _, _) = self.locate(field_name, &TypeRepr::Float, "Float")?;
        self.bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Write a boolean to `field_name`. Encoded as `0u8` / `1u8`.
    pub fn write_bool(&mut self, field_name: &str, value: bool) -> Result<(), BufferError> {
        let (offset, _, _) = self.locate(field_name, &TypeRepr::Bool, "Bool")?;
        self.bytes[offset] = u8::from(value);
        Ok(())
    }

    /// Mark `field_name` as `Null`. The slot is already zeroed by
    /// `new`, so this is a no-op beyond the type check — useful to
    /// surface a `TypeMismatch` early when the host accidentally
    /// nulls a non-Null slot.
    pub fn write_null(&mut self, field_name: &str) -> Result<(), BufferError> {
        let (_, _, _) = self.locate(field_name, &TypeRepr::Null, "Null")?;
        Ok(())
    }

    /// Write a UTF-8 string into `field_name`'s tail-area record.
    ///
    /// Appends `[len: u32 LE][bytes]` after the current buffer end,
    /// padding the cursor up to 4 bytes first so the length prefix is
    /// naturally aligned. The pointer slot in the fixed area is
    /// back-patched with the byte offset of the length prefix
    /// (relative to the buffer base — the wasm side adds `in_ptr` to
    /// reach absolute memory).
    pub fn write_string(&mut self, field_name: &str, value: &str) -> Result<(), BufferError> {
        let (slot_offset, _, kind) = self.locate(field_name, &TypeRepr::String, "String")?;
        if !matches!(kind, FieldKind::PointerIndirect { .. }) {
            // Defensive: the layout pass guarantees this, but we keep
            // the check so a hand-built `OffsetTable` can't sneak an
            // inline String through and corrupt adjacent slots.
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "String",
                requested: "String",
            });
        }
        let len = value.len();
        if len > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: field_name.to_string(),
                len,
            });
        }
        let payload_offset = self.append_tail_record(4, len, value.as_bytes());
        let ptr = u32::try_from(payload_offset).map_err(|_| BufferError::ValueTooLarge {
            name: field_name.to_string(),
            len: payload_offset,
        })?;
        self.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
        Ok(())
    }

    /// Write a `List<Int>` into `field_name`'s tail-area record.
    ///
    /// Tail layout: `[len: u32 LE][i64 LE x len]`. The length prefix
    /// is padded up to 8 bytes after itself so the i64 elements sit
    /// on an 8-byte boundary the way the wasm side will eventually
    /// expect (Phase 2.c keeps the elements untouched, but later
    /// phases reading them need the alignment to be honest).
    pub fn write_list_int(&mut self, field_name: &str, values: &[i64]) -> Result<(), BufferError> {
        let (slot_offset, _, kind) = self.locate(
            field_name,
            &TypeRepr::List {
                element: Box::new(TypeRepr::Int),
            },
            "List<Int>",
        )?;
        let FieldKind::PointerIndirect { tail_alignment } = kind else {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "List<Int>",
                requested: "List<Int>",
            });
        };
        if values.len() > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: field_name.to_string(),
                len: values.len(),
            });
        }
        // Materialise elements into a `Vec<u8>` so `append_tail_record`
        // can copy them in a single slice — avoids reborrowing the
        // builder mid-write.
        let mut payload = Vec::with_capacity(values.len() * 8);
        for v in values {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        let payload_offset =
            self.append_tail_record_with_inner_alignment(4, tail_alignment, values.len(), &payload);
        let ptr = u32::try_from(payload_offset).map_err(|_| BufferError::ValueTooLarge {
            name: field_name.to_string(),
            len: payload_offset,
        })?;
        self.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
        Ok(())
    }

    /// Consume the builder and return the underlying byte buffer.
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn locate(
        &self,
        field_name: &str,
        expected: &TypeRepr,
        requested_label: &'static str,
    ) -> Result<(usize, usize, FieldKind), BufferError> {
        let entry = self
            .field_index
            .iter()
            .find(|(name, _, _, _, _)| name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !type_matches(&entry.1, expected) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.1),
                requested: requested_label,
            });
        }
        Ok((entry.2, entry.3, entry.4))
    }

    /// Append a `[len: u32 LE][payload]` record. Pads the buffer up
    /// to `prefix_alignment` before the length prefix so a future
    /// reader can dereference the pointer without an unaligned load.
    ///
    /// Returns the byte offset of the **length prefix** — the value
    /// that gets back-patched into the fixed-area pointer slot.
    fn append_tail_record(&mut self, prefix_alignment: usize, len: usize, payload: &[u8]) -> usize {
        self.pad_to(prefix_alignment);
        let record_offset = self.bytes.len();
        self.bytes.extend_from_slice(&(len as u32).to_le_bytes());
        self.bytes.extend_from_slice(payload);
        record_offset
    }

    /// Variant of [`Self::append_tail_record`] that pads between the
    /// length prefix and the payload so the payload starts at
    /// `inner_alignment`. `List<Int>` uses this so the i64 elements
    /// sit on an 8-byte boundary the wasm side can load aligned.
    fn append_tail_record_with_inner_alignment(
        &mut self,
        prefix_alignment: usize,
        inner_alignment: usize,
        len: usize,
        payload: &[u8],
    ) -> usize {
        self.pad_to(prefix_alignment);
        let record_offset = self.bytes.len();
        self.bytes.extend_from_slice(&(len as u32).to_le_bytes());
        if inner_alignment > 1 {
            self.pad_to(inner_alignment);
        }
        self.bytes.extend_from_slice(payload);
        record_offset
    }

    /// Grow the buffer with zero bytes until its length is a multiple
    /// of `align`. No-op when already aligned.
    fn pad_to(&mut self, align: usize) {
        if align <= 1 {
            return;
        }
        let rem = self.bytes.len() % align;
        if rem != 0 {
            let pad = align - rem;
            self.bytes.resize(self.bytes.len() + pad, 0);
        }
    }

    /// Read-only accessor used by tests to peek at the layout the
    /// writer is filling — keeps `layout: &OffsetTable` from looking
    /// unused while staying out of the public surface in cases where
    /// callers already have the table.
    #[allow(dead_code)]
    pub(crate) fn layout(&self) -> &OffsetTable {
        self.layout
    }
}

/// Type-checked reader over a record buffer plus optional tail area.
///
/// The buffer is borrowed (no copy), so inline reads cost a bounds
/// check plus a `from_le_bytes`. Pointer-indirect reads follow the
/// `u32` slot through to the tail-area `[len: u32 LE][payload]`
/// record, validating the bounds and (for `String`) the utf-8 bytes
/// against the borrowed buffer.
#[derive(Debug)]
pub struct BufferReader<'a> {
    layout: &'a OffsetTable,
    field_index: Vec<(String, TypeRepr, usize, usize, FieldKind)>,
    bytes: &'a [u8],
}

impl<'a> BufferReader<'a> {
    /// Build a reader over `bytes` interpreting it under `layout`.
    /// Returns [`BufferError::BufferTooSmall`] when `bytes` is shorter
    /// than `layout.root_size` — every leaf read otherwise would have
    /// to repeat the same bounds check, so we do it once at
    /// construction.
    pub fn new(
        layout: &'a OffsetTable,
        fields: &[Field],
        bytes: &'a [u8],
    ) -> Result<Self, BufferError> {
        if bytes.len() < layout.root_size {
            return Err(BufferError::BufferTooSmall {
                have: bytes.len(),
                need: layout.root_size,
            });
        }
        let field_index = layout
            .fields
            .iter()
            .filter_map(|fo| {
                fields
                    .iter()
                    .find(|f| f.name == fo.name)
                    .map(|f| (fo.name.clone(), f.ty.clone(), fo.offset, fo.size, fo.kind))
            })
            .collect();
        Ok(Self {
            layout,
            field_index,
            bytes,
        })
    }

    /// Read a 64-bit signed integer from `field_name`.
    pub fn read_int(&self, field_name: &str) -> Result<i64, BufferError> {
        let (offset, _, _) = self.locate(field_name, &TypeRepr::Int, "Int")?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.bytes[offset..offset + 8]);
        Ok(i64::from_le_bytes(buf))
    }

    /// Read a 64-bit float from `field_name`.
    pub fn read_float(&self, field_name: &str) -> Result<f64, BufferError> {
        let (offset, _, _) = self.locate(field_name, &TypeRepr::Float, "Float")?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.bytes[offset..offset + 8]);
        Ok(f64::from_le_bytes(buf))
    }

    /// Read a boolean from `field_name`. Any non-zero byte decodes
    /// as `true` (the layout only writes 0 or 1, but defensive
    /// decoding makes the reader robust against buffer corruption).
    pub fn read_bool(&self, field_name: &str) -> Result<bool, BufferError> {
        let (offset, _, _) = self.locate(field_name, &TypeRepr::Bool, "Bool")?;
        Ok(self.bytes[offset] != 0)
    }

    /// Confirm `field_name` is declared as `Null` and that the slot
    /// is reachable. The byte value is unused (Null slots are
    /// tag-only), so this only validates the type label.
    pub fn read_null(&self, field_name: &str) -> Result<(), BufferError> {
        let (_, _, _) = self.locate(field_name, &TypeRepr::Null, "Null")?;
        Ok(())
    }

    /// Read a UTF-8 string from `field_name`. Follows the fixed-area
    /// `u32` pointer into the tail area, validates the length prefix
    /// + payload bounds, and decodes the bytes as utf-8.
    pub fn read_string(&self, field_name: &str) -> Result<&'a str, BufferError> {
        let (ptr_offset, _, kind) = self.locate(field_name, &TypeRepr::String, "String")?;
        if !matches!(kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        }
        let (len, payload_start) = self.decode_pointer_header(field_name, ptr_offset, 0)?;
        let payload_end =
            payload_start
                .checked_add(len)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "payload end overflows usize",
                })?;
        if payload_end > self.bytes.len() {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "payload exceeds buffer end",
            });
        }
        std::str::from_utf8(&self.bytes[payload_start..payload_end]).map_err(|_| {
            BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "payload is not valid utf-8",
            }
        })
    }

    /// Read a `List<Int>` from `field_name`. Follows the fixed-area
    /// `u32` pointer into the tail area and copies the i64 elements
    /// into a fresh `Vec<i64>` so callers don't have to wrestle with
    /// alignment of the borrowed slice.
    pub fn read_list_int(&self, field_name: &str) -> Result<Vec<i64>, BufferError> {
        let (ptr_offset, _, kind) = self.locate(
            field_name,
            &TypeRepr::List {
                element: Box::new(TypeRepr::Int),
            },
            "List<Int>",
        )?;
        let FieldKind::PointerIndirect { tail_alignment } = kind else {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        };
        let (count, payload_start) =
            self.decode_pointer_header(field_name, ptr_offset, tail_alignment)?;
        let byte_len = count
            .checked_mul(8)
            .ok_or_else(|| BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "byte length overflows usize",
            })?;
        let payload_end =
            payload_start
                .checked_add(byte_len)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "payload end overflows usize",
                })?;
        if payload_end > self.bytes.len() {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "payload exceeds buffer end",
            });
        }
        let mut out = Vec::with_capacity(count);
        let mut cursor = payload_start;
        for _ in 0..count {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&self.bytes[cursor..cursor + 8]);
            out.push(i64::from_le_bytes(buf));
            cursor += 8;
        }
        Ok(out)
    }

    /// Decode the buffer-relative pointer slot for a nested branded
    /// dict field. Returns a fresh [`BufferReader`] anchored at the
    /// sub-record's fixed-area base, sharing the parent's underlying
    /// byte buffer.
    ///
    /// Phase 3.b: branded dict fields lay their fixed area in the
    /// parent's tail area, addressed through a 4-byte pointer slot in
    /// the parent's fixed area. The sub-record's own pointer-indirect
    /// children (its String / List<Int> / nested Dict slots) keep
    /// pointing into the same shared buffer — `sub_record` borrows the
    /// parent bytes verbatim, so a subsequent `read_string` on the
    /// sub-reader resolves through the same tail area without copying.
    ///
    /// `sub_layout` is the [`OffsetTable`] for the sub-schema (e.g.
    /// computed via [`crate::layout::SchemaLayout::offsets_for`]).
    /// `sub_fields` is the schema-declared field list — pass
    /// `sub_schema.fields.as_slice()` so the returned reader can
    /// type-check its own field accesses.
    pub fn sub_record(
        &self,
        field_name: &str,
        sub_layout: &'a OffsetTable,
        sub_fields: &[Field],
    ) -> Result<BufferReader<'a>, BufferError> {
        // Locate the parent's pointer slot. We don't constrain the
        // declared `TypeRepr` here because the caller already knows
        // the sub-schema — re-matching it would duplicate the schema
        // walker. Use a wildcard type check via an opt-in helper to
        // get the offset + kind back.
        let entry = self
            .field_index
            .iter()
            .find(|(name, _, _, _, _)| name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !matches!(entry.1, TypeRepr::Schema { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.1),
                requested: "Schema",
            });
        }
        if !matches!(entry.4, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        }
        let ptr_offset = entry.2;
        if ptr_offset
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "pointer slot exceeds buffer end",
            });
        }
        let mut ptr_buf = [0u8; 4];
        ptr_buf.copy_from_slice(&self.bytes[ptr_offset..ptr_offset + 4]);
        let sub_base = u32::from_le_bytes(ptr_buf) as usize;
        let sub_end =
            sub_base
                .checked_add(sub_layout.root_size)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "sub-record end overflows usize",
                })?;
        if sub_end > self.bytes.len() {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "sub-record exceeds buffer end",
            });
        }
        // The sub-reader walks the **same** underlying buffer slice —
        // not a slice starting at `sub_base`. Its `bytes` covers the
        // whole parent buffer because the sub-record's own pointer-
        // indirect slots store offsets relative to the buffer base.
        // We instead re-base each field by adding `sub_base` to the
        // declared offset, which `BufferReader::new` doesn't do for
        // us — so we build the field-index manually.
        let field_index = sub_layout
            .fields
            .iter()
            .filter_map(|fo| {
                sub_fields.iter().find(|f| f.name == fo.name).map(|f| {
                    (
                        fo.name.clone(),
                        f.ty.clone(),
                        sub_base + fo.offset,
                        fo.size,
                        fo.kind,
                    )
                })
            })
            .collect();
        Ok(BufferReader {
            layout: sub_layout,
            field_index,
            bytes: self.bytes,
        })
    }

    /// Resolve a fixed-area `u32` pointer slot into `(payload_count,
    /// payload_byte_offset)`. `inner_alignment` controls the padding
    /// between the length prefix and the payload bytes — `String`
    /// passes `0` (no padding); `List<Int>` passes `8`.
    fn decode_pointer_header(
        &self,
        field_name: &str,
        ptr_offset: usize,
        inner_alignment: usize,
    ) -> Result<(usize, usize), BufferError> {
        if ptr_offset
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "pointer slot exceeds buffer end",
            });
        }
        let mut ptr_buf = [0u8; 4];
        ptr_buf.copy_from_slice(&self.bytes[ptr_offset..ptr_offset + 4]);
        let record_start = u32::from_le_bytes(ptr_buf) as usize;
        if record_start
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "length prefix exceeds buffer end",
            });
        }
        let mut len_buf = [0u8; 4];
        len_buf.copy_from_slice(&self.bytes[record_start..record_start + 4]);
        let count = u32::from_le_bytes(len_buf) as usize;
        let payload_start_raw = record_start + 4;
        let payload_start = if inner_alignment > 1 {
            let rem = payload_start_raw % inner_alignment;
            if rem == 0 {
                payload_start_raw
            } else {
                payload_start_raw
                    .checked_add(inner_alignment - rem)
                    .ok_or_else(|| BufferError::MalformedPayload {
                        name: field_name.to_string(),
                        reason: "payload start overflows usize",
                    })?
            }
        } else {
            payload_start_raw
        };
        Ok((count, payload_start))
    }

    fn locate(
        &self,
        field_name: &str,
        expected: &TypeRepr,
        requested_label: &'static str,
    ) -> Result<(usize, usize, FieldKind), BufferError> {
        let entry = self
            .field_index
            .iter()
            .find(|(name, _, _, _, _)| name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !type_matches(&entry.1, expected) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.1),
                requested: requested_label,
            });
        }
        Ok((entry.2, entry.3, entry.4))
    }

    /// Read-only access to the layout this reader walks.
    #[allow(dead_code)]
    pub(crate) fn layout(&self) -> &OffsetTable {
        self.layout
    }
}

/// Compare a schema-declared [`TypeRepr`] against the one a writer /
/// reader assumed. Phase 3.b extends the match set to include nested
/// branded `Schema { ... }` slots — the sub-record reader compares
/// schemas by structural equality of the canonical form.
fn type_matches(declared: &TypeRepr, requested: &TypeRepr) -> bool {
    match (declared, requested) {
        (TypeRepr::Int, TypeRepr::Int)
        | (TypeRepr::Float, TypeRepr::Float)
        | (TypeRepr::Bool, TypeRepr::Bool)
        | (TypeRepr::Null, TypeRepr::Null)
        | (TypeRepr::String, TypeRepr::String) => true,
        (TypeRepr::List { element: d }, TypeRepr::List { element: r }) => {
            matches!((d.as_ref(), r.as_ref()), (TypeRepr::Int, TypeRepr::Int))
        }
        (TypeRepr::Schema { schema: d }, TypeRepr::Schema { schema: r }) => d == r,
        _ => false,
    }
}

/// Human-readable label for the schema's declared type. Used in the
/// `TypeMismatch` error path so users see "Int" rather than `Debug`-
/// formatted `TypeRepr::Int`.
fn type_label(ty: &TypeRepr) -> &'static str {
    match ty {
        TypeRepr::Null => "Null",
        TypeRepr::Bool => "Bool",
        TypeRepr::Int => "Int",
        TypeRepr::Float => "Float",
        TypeRepr::String => "String",
        TypeRepr::List { .. } => "List",
        TypeRepr::Option { .. } => "Option",
        TypeRepr::Result { .. } => "Result",
        TypeRepr::Schema { .. } => "Schema",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SchemaLayout;
    use crate::schema_canonical::{Field, Schema};

    fn field(name: &str, ty: TypeRepr) -> Field {
        Field {
            name: name.into(),
            ty,
            default: None,
        }
    }

    fn int_schema() -> Schema {
        Schema {
            name: "Pair".into(),
            generics: vec![],
            fields: vec![field("x", TypeRepr::Int), field("y", TypeRepr::Int)],
        }
    }

    fn mixed_schema() -> Schema {
        Schema {
            name: "Mix".into(),
            generics: vec![],
            fields: vec![
                field("count", TypeRepr::Int),
                field("active", TypeRepr::Bool),
            ],
        }
    }

    #[test]
    fn write_int_then_read_back_roundtrips() {
        let schema = int_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_int("x", 42).expect("write x");
        builder.write_int("y", -7).expect("write y");
        let bytes = builder.finish();
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(reader.read_int("x").expect("read x"), 42);
        assert_eq!(reader.read_int("y").expect("read y"), -7);
    }

    #[test]
    fn mixed_int_bool_roundtrip_respects_padding() {
        let schema = mixed_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_int("count", 100).expect("write count");
        builder.write_bool("active", true).expect("write active");
        let bytes = builder.finish();

        // Buffer is exactly root_size wide; padding lives at offsets
        // 9..16. The reader doesn't care about padding contents.
        assert_eq!(bytes.len(), layout.root_size);

        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(reader.read_int("count").expect("read count"), 100);
        assert!(reader.read_bool("active").expect("read active"));
    }

    #[test]
    fn unknown_field_is_rejected() {
        let schema = int_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        let err = builder
            .write_int("missing", 1)
            .expect_err("unknown field must reject");
        assert!(matches!(err, BufferError::UnknownField { ref name } if name == "missing"));
    }

    #[test]
    fn type_mismatch_is_rejected() {
        // Bool slot accessed via write_int — would corrupt adjacent
        // fields if the writer silently accepted it.
        let schema = mixed_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        let err = builder
            .write_int("active", 1)
            .expect_err("type mismatch must reject");
        assert!(matches!(
            err,
            BufferError::TypeMismatch {
                declared: "Bool",
                requested: "Int",
                ..
            }
        ));
    }

    #[test]
    fn reader_rejects_short_buffer() {
        let schema = mixed_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let short = vec![0u8; layout.root_size - 1];
        let err = BufferReader::new(&layout, &schema.fields, &short)
            .expect_err("short buffer must reject");
        assert!(matches!(
            err,
            BufferError::BufferTooSmall { have, need }
            if have == layout.root_size - 1 && need == layout.root_size
        ));
    }

    #[test]
    fn float_and_null_roundtrip() {
        let schema = Schema {
            name: "Phys".into(),
            generics: vec![],
            fields: vec![field("mass", TypeRepr::Float), field("nil", TypeRepr::Null)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_float("mass", 1.5_f64).expect("write mass");
        builder.write_null("nil").expect("write nil");
        let bytes = builder.finish();
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(reader.read_float("mass").expect("read mass"), 1.5);
        reader.read_null("nil").expect("read nil");
    }

    #[test]
    fn write_string_then_read_back_roundtrips() {
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_string("name", "hello").expect("write name");
        let bytes = builder.finish();
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(reader.read_string("name").expect("read name"), "hello");
    }

    #[test]
    fn empty_string_roundtrips() {
        // Zero-byte payload still gets a length prefix, so the reader
        // must walk through `[len=0][]` without spuriously claiming
        // out-of-bounds.
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_string("name", "").expect("write empty");
        let bytes = builder.finish();
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(reader.read_string("name").expect("read name"), "");
    }

    #[test]
    fn write_string_then_int_fixed_area_lays_out_correctly() {
        // Pointer slot at 0..4, padding 4..8, Int slot at 8..16. The
        // tail-area record sits at offset 16 (or later if padded).
        // We verify the layout via direct byte inspection so a
        // regression in the slot order surfaces immediately.
        let schema = Schema {
            name: "User".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String), field("age", TypeRepr::Int)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_string("name", "ada").expect("write name");
        builder.write_int("age", 36).expect("write age");
        let bytes = builder.finish();

        // First 4 bytes hold the tail-area pointer; bytes 8..16 hold
        // the Int slot regardless of how big the tail record is.
        let ptr = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert!(ptr >= layout.root_size);
        let len_prefix = u32::from_le_bytes(bytes[ptr..ptr + 4].try_into().unwrap()) as usize;
        assert_eq!(len_prefix, "ada".len());
        let age = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
        assert_eq!(age, 36);

        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(reader.read_string("name").expect("read name"), "ada");
        assert_eq!(reader.read_int("age").expect("read age"), 36);
    }

    #[test]
    fn unknown_field_on_write_string_is_rejected() {
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        let err = builder
            .write_string("missing", "x")
            .expect_err("unknown field must reject");
        assert!(matches!(err, BufferError::UnknownField { ref name } if name == "missing"));
    }

    #[test]
    fn write_list_int_then_read_back_roundtrips() {
        let schema = Schema {
            name: "Nums".into(),
            generics: vec![],
            fields: vec![field(
                "nums",
                TypeRepr::List {
                    element: Box::new(TypeRepr::Int),
                },
            )],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder
            .write_list_int("nums", &[1, -2, 3])
            .expect("write nums");
        let bytes = builder.finish();
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(
            reader.read_list_int("nums").expect("read nums"),
            vec![1, -2, 3]
        );
    }

    #[test]
    fn buffer_too_small_on_string_schema_rejected() {
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let short = vec![0u8; layout.root_size - 1];
        let err = BufferReader::new(&layout, &schema.fields, &short)
            .expect_err("short buffer must reject");
        assert!(matches!(err, BufferError::BufferTooSmall { .. }));
    }

    /// Hand-build a buffer matching the host->wasm wire shape for an
    /// outer `Usr { Addr addr, String name }` record so the
    /// `sub_record` reader test exercises both a nested record and a
    /// trailing String alongside it. Keeps the layout details in one
    /// place — the asserts focus on the reader returning the right
    /// values rather than the exact tail-area byte arithmetic.
    fn build_usr_buffer() -> (Schema, Schema, Vec<u8>) {
        let addr_schema = Schema {
            name: "Addr".into(),
            generics: vec![],
            fields: vec![field("city", TypeRepr::String), field("zip", TypeRepr::Int)],
        };
        let usr_schema = Schema {
            name: "Usr".into(),
            generics: vec![],
            fields: vec![
                field(
                    "addr",
                    TypeRepr::Schema {
                        schema: Box::new(addr_schema.clone()),
                    },
                ),
                field("name", TypeRepr::String),
            ],
        };
        let usr_layout = SchemaLayout::offsets_for(&usr_schema).expect("usr layout");
        let addr_layout = SchemaLayout::offsets_for(&addr_schema).expect("addr layout");

        // Fixed area sizes: Usr.root_size = 8 (two 4-byte pointers);
        // Addr.root_size = 16 (4-byte ptr + 4 pad + 8 int). Bytes are
        // assembled by hand so the layout invariants stay visible at
        // the call site of the test.
        let usr_root = usr_layout.root_size;
        assert_eq!(usr_root, 8);
        assert_eq!(addr_layout.root_size, 16);

        let mut bytes = vec![0u8; usr_root];

        // Sub-record Addr fixed area lives in the tail at offset =
        // usr_root, padded up to addr_layout.root_align (=8). 8 is
        // already aligned.
        let addr_base = bytes.len();
        bytes.resize(addr_base + addr_layout.root_size, 0);

        // String "BJ" tail record at offset following Addr.
        let bj_offset = bytes.len();
        bytes.extend_from_slice(&(2u32).to_le_bytes());
        bytes.extend_from_slice(b"BJ");
        // Patch Addr.city pointer (offset 0 inside Addr).
        let bj_ptr = bj_offset as u32;
        bytes[addr_base..addr_base + 4].copy_from_slice(&bj_ptr.to_le_bytes());
        // Patch Addr.zip = 100000 at offset 8 inside Addr.
        bytes[addr_base + 8..addr_base + 16].copy_from_slice(&100000i64.to_le_bytes());

        // String "Bob" tail record. Pad up to 4-byte boundary first
        // since the previous "BJ" ended at an odd byte position.
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
        let bob_offset = bytes.len();
        bytes.extend_from_slice(&(3u32).to_le_bytes());
        bytes.extend_from_slice(b"Bob");

        // Patch the Usr fixed area: addr pointer slot at offset 0,
        // name pointer slot at offset 4.
        let addr_ptr = addr_base as u32;
        bytes[0..4].copy_from_slice(&addr_ptr.to_le_bytes());
        bytes[4..8].copy_from_slice(&(bob_offset as u32).to_le_bytes());

        (usr_schema, addr_schema, bytes)
    }

    #[test]
    fn sub_record_reads_nested_dict_fields() {
        let (usr_schema, addr_schema, bytes) = build_usr_buffer();
        let usr_layout = SchemaLayout::offsets_for(&usr_schema).expect("usr layout");
        let addr_layout = SchemaLayout::offsets_for(&addr_schema).expect("addr layout");
        let reader = BufferReader::new(&usr_layout, &usr_schema.fields, &bytes).expect("usr");

        let sub = reader
            .sub_record("addr", &addr_layout, &addr_schema.fields)
            .expect("sub");
        assert_eq!(sub.read_string("city").expect("city"), "BJ");
        assert_eq!(sub.read_int("zip").expect("zip"), 100000);
        // Parent reader still reaches the trailing top-level String.
        assert_eq!(reader.read_string("name").expect("name"), "Bob");
    }

    #[test]
    fn sub_record_rejects_non_schema_field() {
        // `name` is a String, not a sub-schema — sub_record must
        // refuse rather than read random bytes out of the buffer.
        let (usr_schema, addr_schema, bytes) = build_usr_buffer();
        let usr_layout = SchemaLayout::offsets_for(&usr_schema).expect("usr layout");
        let addr_layout = SchemaLayout::offsets_for(&addr_schema).expect("addr layout");
        let reader = BufferReader::new(&usr_layout, &usr_schema.fields, &bytes).expect("usr");
        let err = reader
            .sub_record("name", &addr_layout, &addr_schema.fields)
            .expect_err("non-schema slot");
        assert!(matches!(
            err,
            BufferError::TypeMismatch {
                requested: "Schema",
                ..
            }
        ));
    }

    #[test]
    fn sub_record_unknown_field_rejected() {
        let (usr_schema, addr_schema, bytes) = build_usr_buffer();
        let usr_layout = SchemaLayout::offsets_for(&usr_schema).expect("usr layout");
        let addr_layout = SchemaLayout::offsets_for(&addr_schema).expect("addr layout");
        let reader = BufferReader::new(&usr_layout, &usr_schema.fields, &bytes).expect("usr");
        let err = reader
            .sub_record("missing", &addr_layout, &addr_schema.fields)
            .expect_err("missing");
        assert!(matches!(err, BufferError::UnknownField { .. }));
    }

    #[test]
    fn nested_schema_layout_picks_inner_alignment() {
        // Schema { String s, Int i } pulls root_align up to 8, so a
        // parent slot referencing it lays out as a pointer-indirect
        // field with `tail_alignment: 8`.
        let inner = Schema {
            name: "Inner".into(),
            generics: vec![],
            fields: vec![field("s", TypeRepr::String), field("i", TypeRepr::Int)],
        };
        let outer = Schema {
            name: "Outer".into(),
            generics: vec![],
            fields: vec![field(
                "child",
                TypeRepr::Schema {
                    schema: Box::new(inner),
                },
            )],
        };
        let table = SchemaLayout::offsets_for(&outer).expect("layout");
        let kind = table.fields[0].kind;
        assert!(matches!(
            kind,
            FieldKind::PointerIndirect { tail_alignment: 8 }
        ));
    }

    #[test]
    fn type_mismatch_on_read_is_rejected() {
        // Symmetric to the writer test — guards against a host doing
        // `read_bool` on an Int slot and getting back random bytes.
        let schema = mixed_schema();
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let bytes = vec![0u8; layout.root_size];
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        let err = reader
            .read_bool("count")
            .expect_err("type mismatch on read must reject");
        assert!(matches!(
            err,
            BufferError::TypeMismatch {
                declared: "Int",
                requested: "Bool",
                ..
            }
        ));
    }
}
