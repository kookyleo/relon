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

use crate::layout::{FieldKind, ListElementKind, OffsetTable, SchemaLayout};
use crate::schema_canonical::{Field, Schema, TypeRepr};
use std::sync::Arc;
use thiserror::Error;

/// One row in the per-builder / per-reader field index. Carries the
/// declared schema type alongside the offset-table data so writers
/// and readers can dispatch on declared shape without re-walking the
/// schema. Phase 10-c added `list_element` for `List<T>` dispatch.
#[derive(Debug, Clone)]
struct FieldEntry {
    name: String,
    ty: TypeRepr,
    offset: usize,
    size: usize,
    kind: FieldKind,
    list_element: Option<ListElementKind>,
}

/// One slot in a [`RelocLayout`] — every entry maps 1:1 to a
/// `PointerIndirect` field in the parent [`OffsetTable`]. Inline fields
/// don't need relocation and are filtered out at build time so the
/// relocation walker can skip them without a `matches!` check per slot.
#[derive(Debug)]
pub(crate) struct RelocSlot {
    /// Byte offset of the pointer slot within the record's fixed area.
    offset: usize,
    /// Per-element layout for `List<T>` fields, mirroring
    /// [`FieldOffset::list_element`]. Drives the dispatch between the
    /// pointer-array relocator and the inline schema recursion.
    list_element: Option<ListElementKind>,
    /// Sub-layout for nested `Schema` / `List<Schema>` fields. Pre-computed
    /// at builder construction so relocation never has to call
    /// [`SchemaLayout::offsets_for`] on the hot path. `None` for
    /// `String` / `List<scalar>` slots where the tail record carries no
    /// further pointers.
    nested: Option<Arc<RelocLayout>>,
}

/// Pre-computed relocation table for a [`BufferBuilder`]'s schema.
///
/// `relocate_pointers` walks pointer-indirect slots when a child buffer
/// is pasted into a parent (see [`BufferBuilder::finish_sub_record`] and
/// [`ListRecordWriter::finish_entry`]). Each nested `Schema` / `List<Schema>`
/// slot needs its own [`OffsetTable`] to recurse through; computing it
/// via [`SchemaLayout::offsets_for`] on every relocation made the
/// `List<Schema>` / `Dict<_, Schema>` paths quadratic in nesting depth
/// and quadratic in entry count. The cache shifts that work to a single
/// up-front walk at [`BufferBuilder::new`] / [`ListRecordWriter`]
/// construction.
///
/// Layout: `slots` mirrors the `PointerIndirect` fields in
/// [`OffsetTable::fields`]; each [`RelocSlot::nested`] is `Some` exactly
/// when the field's declared type is `Schema { .. }` or
/// `List { element: Schema { .. } }`, and points at a sibling
/// `RelocLayout` for the inner record. `Arc` lets identical sub-trees
/// share a single allocation when the same nested schema appears at
/// multiple sites (e.g. two `List<User>` fields sharing the inner
/// `User` layout).
#[derive(Debug)]
pub(crate) struct RelocLayout {
    slots: Vec<RelocSlot>,
}

impl RelocLayout {
    /// Build the relocation cache for one schema layer.
    ///
    /// Walks `layout.fields` and for every `PointerIndirect` slot whose
    /// declared type is `Schema` / `List<Schema>`, recursively computes
    /// the nested layout via [`SchemaLayout::offsets_for`] **once**.
    /// Subsequent relocations reuse the cached layouts via the returned
    /// `Arc`. Inline slots (`Int`, `Bool`, ...) are skipped — they're
    /// never touched by `relocate_pointers`.
    fn build(layout: &OffsetTable, fields: &[Field]) -> Arc<Self> {
        let mut slots: Vec<RelocSlot> = Vec::new();
        for fo in &layout.fields {
            if !matches!(fo.kind, FieldKind::PointerIndirect { .. }) {
                continue;
            }
            let declared = fields.iter().find(|f| f.name == fo.name);
            let nested = declared.and_then(|f| match &f.ty {
                TypeRepr::Schema { schema } => SchemaLayout::offsets_for(schema)
                    .ok()
                    .map(|sub| RelocLayout::build(&sub, &schema.fields)),
                TypeRepr::List { element } => match element.as_ref() {
                    TypeRepr::Schema { schema } => SchemaLayout::offsets_for(schema)
                        .ok()
                        .map(|sub| RelocLayout::build(&sub, &schema.fields)),
                    _ => None,
                },
                _ => None,
            });
            slots.push(RelocSlot {
                offset: fo.offset,
                list_element: fo.list_element,
                nested,
            });
        }
        Arc::new(Self { slots })
    }
}

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
    /// A pointer-indirect payload (String / `List<Int>`) is larger than
    /// the `u32` length prefix can describe. Phase 2.c caps each
    /// payload at `u32::MAX` bytes / elements; longer values surface
    /// here rather than overflow silently.
    #[error("payload for field `{name}` is too large: {len} exceeds u32::MAX")]
    ValueTooLarge {
        /// Field name carrying the oversized payload.
        name: String,
        /// Requested length (bytes for String, elements for `List<Int>`).
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
/// * String / `List<Int>` writes append a `[len: u32 LE][payload]`
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
    field_index: Vec<FieldEntry>,
    bytes: Vec<u8>,
    /// Pre-computed relocation cache for `relocate_pointers`. Built once
    /// at `new` time so subsequent pastes into a parent buffer (via
    /// `finish_sub_record` / `finish_entry`) never re-walk the nested
    /// schemas. Shared via `Arc` so a `ListRecordWriter`'s entry builders
    /// hand the same cache back to the parent without re-deriving it.
    reloc_layout: Arc<RelocLayout>,
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
                    .map(|f| FieldEntry {
                        name: fo.name.clone(),
                        ty: f.ty.clone(),
                        offset: fo.offset,
                        size: fo.size,
                        kind: fo.kind,
                        list_element: fo.list_element,
                    })
            })
            .collect();
        let reloc_layout = RelocLayout::build(layout, fields);
        Self {
            layout,
            field_index,
            bytes,
            reloc_layout,
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

    /// Write a `List<Float>` into `field_name`'s tail-area record.
    ///
    /// Phase 10-c: tail layout mirrors `List<Int>` — `[len: u32 LE]
    /// [pad to 8][f64 LE x len]`. The post-len pad keeps the f64
    /// payload on an 8-byte boundary so the wasm side can issue
    /// `f64.load align=3` against the element stream.
    pub fn write_list_float(
        &mut self,
        field_name: &str,
        values: &[f64],
    ) -> Result<(), BufferError> {
        let (slot_offset, kind, _elem) =
            self.locate_list(field_name, &TypeRepr::Float, "List<Float>")?;
        let FieldKind::PointerIndirect { tail_alignment } = kind else {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "List<Float>",
                requested: "List<Float>",
            });
        };
        if values.len() > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: field_name.to_string(),
                len: values.len(),
            });
        }
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

    /// Write a `List<Bool>` into `field_name`'s tail-area record.
    ///
    /// Phase 10-c: tail layout `[len: u32 LE][u8 x len]` — booleans
    /// pack tightly with no inter-element padding per spec. The
    /// record start is 4-byte aligned so the len prefix loads cleanly;
    /// each element is one byte (`0` for false, `1` for true).
    pub fn write_list_bool(
        &mut self,
        field_name: &str,
        values: &[bool],
    ) -> Result<(), BufferError> {
        let (slot_offset, kind, _elem) =
            self.locate_list(field_name, &TypeRepr::Bool, "List<Bool>")?;
        if !matches!(kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "List<Bool>",
                requested: "List<Bool>",
            });
        }
        if values.len() > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: field_name.to_string(),
                len: values.len(),
            });
        }
        let payload: Vec<u8> = values.iter().map(|&b| u8::from(b)).collect();
        let payload_offset = self.append_tail_record(4, values.len(), &payload);
        let ptr = u32::try_from(payload_offset).map_err(|_| BufferError::ValueTooLarge {
            name: field_name.to_string(),
            len: payload_offset,
        })?;
        self.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
        Ok(())
    }

    /// Write a `List<String>` into `field_name`'s tail-area record.
    ///
    /// Phase 10-c: header `[len: u32 LE][off_0: u32 LE]...[off_(n-1)]`
    /// followed by per-string `[len: u32 LE][utf8 bytes]` tail records.
    /// Each `off_i` is the buffer-relative byte offset of the matching
    /// String's len prefix; the writer pads each String header to a
    /// 4-byte boundary so the reader can dereference without an
    /// unaligned load.
    pub fn write_list_string<S: AsRef<str>>(
        &mut self,
        field_name: &str,
        values: &[S],
    ) -> Result<(), BufferError> {
        let (slot_offset, kind, _elem) =
            self.locate_list(field_name, &TypeRepr::String, "List<String>")?;
        if !matches!(kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "List<String>",
                requested: "List<String>",
            });
        }
        let count = values.len();
        if count > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: field_name.to_string(),
                len: count,
            });
        }
        // Allocate the header `[len][off_0]...[off_(n-1)]` first, with
        // zeroed entries we'll back-patch as each String tail record
        // lands. The header itself is 4-byte aligned.
        self.pad_to(4);
        let header_offset = self.bytes.len();
        self.bytes
            .extend_from_slice(&u32::try_from(count).unwrap().to_le_bytes());
        let entries_start = self.bytes.len();
        self.bytes.resize(entries_start + count * 4, 0);
        // Append each String tail record, capturing its offset.
        let mut offsets: Vec<u32> = Vec::with_capacity(count);
        for (i, s) in values.iter().enumerate() {
            let bytes = s.as_ref().as_bytes();
            if bytes.len() > u32::MAX as usize {
                return Err(BufferError::ValueTooLarge {
                    name: format!("{field_name}[{i}]"),
                    len: bytes.len(),
                });
            }
            let entry_offset = self.append_tail_record(4, bytes.len(), bytes);
            let entry_u32 =
                u32::try_from(entry_offset).map_err(|_| BufferError::ValueTooLarge {
                    name: field_name.to_string(),
                    len: entry_offset,
                })?;
            offsets.push(entry_u32);
        }
        // Back-patch the pointer-array entries with the actual offsets.
        for (i, off) in offsets.iter().enumerate() {
            let dst = entries_start + i * 4;
            self.bytes[dst..dst + 4].copy_from_slice(&off.to_le_bytes());
        }
        // Back-patch the field's pointer slot to the list-header
        // offset (the `len` prefix).
        let ptr = u32::try_from(header_offset).map_err(|_| BufferError::ValueTooLarge {
            name: field_name.to_string(),
            len: header_offset,
        })?;
        self.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
        Ok(())
    }

    /// Start writing a `List<Schema>` element. Returns a list-record
    /// writer the caller drives one entry at a time; the actual list
    /// header pointer is patched into the parent slot when
    /// [`Self::finish_list_record`] is called.
    ///
    /// Phase 10-c: each element is a branded sub-record (the inner
    /// `TypeRepr::Schema { schema }`) whose fixed area lives in the
    /// parent buffer's tail area, addressed by a `u32` entry in the
    /// pointer array. The parent's pointer slot in turn holds the
    /// buffer-relative offset of the list header (`[len: u32][off_0]
    /// ...`).
    ///
    /// Workflow:
    ///
    /// ```ignore
    /// let mut lw = parent.list_record(&field_name, &elem_layout, &elem_schema.fields)?;
    /// for entry in entries {
    ///     let mut child = lw.start_entry(&parent_builder)?;
    ///     // ... write_int / write_string into `child` ...
    ///     lw.finish_entry(&mut parent_builder, child)?;
    /// }
    /// parent.finish_list_record(&field_name, lw)?;
    /// ```
    ///
    /// The split workflow keeps the parent buffer mutable for the
    /// per-entry tail copy without aliasing the child borrow against
    /// the parent's `field_index`. Hosts that don't need the full
    /// step-by-step control can use the [`Self::write_list_record`]
    /// convenience wrapper which takes a slice of pre-built dicts.
    pub fn list_record_writer<'b>(
        &self,
        field_name: &str,
        elem_layout: &'b OffsetTable,
        elem_schema: &'b Schema,
    ) -> Result<ListRecordWriter<'b>, BufferError> {
        let entry = self
            .field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        match &entry.ty {
            TypeRepr::List { element } => match element.as_ref() {
                TypeRepr::Schema { schema } => {
                    if schema.as_ref() != elem_schema {
                        return Err(BufferError::TypeMismatch {
                            name: field_name.to_string(),
                            declared: type_label(&entry.ty),
                            requested: "List<Schema>",
                        });
                    }
                }
                _ => {
                    return Err(BufferError::TypeMismatch {
                        name: field_name.to_string(),
                        declared: type_label(&entry.ty),
                        requested: "List<Schema>",
                    });
                }
            },
            other => {
                return Err(BufferError::TypeMismatch {
                    name: field_name.to_string(),
                    declared: type_label(other),
                    requested: "List<Schema>",
                });
            }
        };
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "List<Schema>",
                requested: "List<Schema>",
            });
        }
        Ok(ListRecordWriter {
            field_name: field_name.to_string(),
            slot_offset: entry.offset,
            elem_layout,
            elem_schema,
            entry_offsets: Vec::new(),
            elem_align: elem_layout.root_align.max(1),
        })
    }

    /// Convenience writer for `List<Schema>` that builds each entry
    /// from a pre-prepared `Vec<(field_name, write_callback)>` shape.
    /// Phase 10-c: tests use the longer-form [`Self::list_record_writer`]
    /// for full control; this wrapper accepts a list of buffer-builder
    /// "actions" so simple cases don't need to spell out the start /
    /// finish dance.
    pub fn write_list_record<'b, F>(
        &mut self,
        field_name: &str,
        elem_layout: &'b OffsetTable,
        elem_schema: &'b Schema,
        entries: &[F],
    ) -> Result<(), BufferError>
    where
        F: Fn(&mut BufferBuilder<'b>) -> Result<(), BufferError>,
    {
        let mut writer = self.list_record_writer(field_name, elem_layout, elem_schema)?;
        for action in entries {
            let mut child = writer.start_entry();
            action(&mut child)?;
            writer.finish_entry(self, child)?;
        }
        self.finish_list_record(writer)
    }

    /// Commit a [`ListRecordWriter`] — emit the list header into the
    /// tail area (aligned to 4) and patch the field's pointer slot
    /// with the header offset.
    pub fn finish_list_record(&mut self, writer: ListRecordWriter<'_>) -> Result<(), BufferError> {
        let count = writer.entry_offsets.len();
        if count > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: writer.field_name,
                len: count,
            });
        }
        self.pad_to(4);
        let header_offset = self.bytes.len();
        self.bytes
            .extend_from_slice(&u32::try_from(count).unwrap().to_le_bytes());
        for off in &writer.entry_offsets {
            self.bytes.extend_from_slice(&off.to_le_bytes());
        }
        let ptr = u32::try_from(header_offset).map_err(|_| BufferError::ValueTooLarge {
            name: writer.field_name.clone(),
            len: header_offset,
        })?;
        let slot_offset = writer.slot_offset;
        self.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
        Ok(())
    }

    /// Consume the builder and return the underlying byte buffer.
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    /// Internal sibling of [`Self::finish`] that surrenders the byte
    /// buffer together with the pre-computed relocation cache.
    /// `finish_sub_record` and `ListRecordWriter::finish_entry` use this
    /// so the relocation walker can skip a fresh `OffsetTable` derivation
    /// per paste — the cache was already built at `new` time.
    pub(crate) fn into_parts(self) -> (Vec<u8>, Arc<RelocLayout>) {
        (self.bytes, self.reloc_layout)
    }

    /// Allocate a nested branded sub-record under `field_name` and
    /// return a detached child [`BufferBuilder`] sized to the sub
    /// schema's fixed area.
    ///
    /// Phase 9.b-1: mirrors [`BufferReader::sub_record`] on the writer
    /// side so a host can pack Schema-typed `#main` args without
    /// reaching for hand-rolled byte arithmetic. The returned builder
    /// is *detached* — it owns its own `Vec<u8>` pre-sized to
    /// `sub_layout.root_size`. The parent's pointer slot stays zero
    /// until the caller hands the child back via
    /// [`Self::finish_sub_record`], which appends the child's bytes to
    /// the parent's tail area (aligning to `sub_layout.root_align`) and
    /// back-patches the slot.
    ///
    /// Detached children keep the writer simple: they don't borrow the
    /// parent (so multiple sibling sub-records can be authored
    /// independently), and the parent has a single commit step that
    /// also enforces the field-name → pointer-slot binding the layout
    /// pass guarantees.
    pub fn sub_record<'b>(
        &mut self,
        field_name: &str,
        sub_layout: &'b OffsetTable,
        sub_fields: &[Field],
    ) -> Result<BufferBuilder<'b>, BufferError> {
        // Validate the parent slot is a Schema-typed pointer-indirect
        // entry. Anything else would corrupt adjacent fields once we
        // back-patched the (wrong) slot.
        let entry = self
            .field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !matches!(entry.ty, TypeRepr::Schema { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: "Schema",
            });
        }
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "Schema",
                requested: "Schema",
            });
        }
        Ok(BufferBuilder::new(sub_layout, sub_fields))
    }

    /// Commit a detached sub-record produced by [`Self::sub_record`].
    ///
    /// Appends the child's byte buffer to the parent's tail area
    /// (padded up to the sub schema's root alignment) and writes the
    /// resulting buffer-relative offset into the parent's pointer slot
    /// for `field_name`. The child is consumed.
    ///
    /// Pointer relocation: the child built its `String` / `List<Int>` /
    /// nested-`Schema` slots with offsets relative to the **child's**
    /// buffer base (0). Once the child is pasted into the parent at
    /// `sub_base`, every such pointer slot needs `+ sub_base` to become
    /// parent-relative again — otherwise the wasm side / reader walks
    /// the wrong bytes. We walk the child's field layout recursively
    /// and rewrite each u32 pointer in place before appending.
    ///
    /// Errors mirror the parent's other writers: an unknown field name
    /// or a type-shape mismatch surfaces before any bytes are moved.
    /// An oversized child (offset doesn't fit in `u32`) surfaces as
    /// [`BufferError::ValueTooLarge`].
    pub fn finish_sub_record(
        &mut self,
        field_name: &str,
        child: BufferBuilder<'_>,
    ) -> Result<(), BufferError> {
        let entry = self
            .field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !matches!(entry.ty, TypeRepr::Schema { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: "Schema",
            });
        }
        let FieldKind::PointerIndirect { tail_alignment } = entry.kind else {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "Schema",
                requested: "Schema",
            });
        };
        let slot_offset = entry.offset;
        let child_align = child.layout.root_align.max(tail_alignment).max(1);
        let (child_bytes, child_reloc) = child.into_parts();
        let mut child_bytes = child_bytes;
        self.pad_to(child_align);
        let sub_base = self.bytes.len();
        let ptr = u32::try_from(sub_base).map_err(|_| BufferError::ValueTooLarge {
            name: field_name.to_string(),
            len: sub_base,
        })?;
        // Rebase every pointer slot inside the child so it's
        // parent-relative once we paste the child bytes.
        relocate_pointers(&mut child_bytes, &child_reloc, 0, ptr).map_err(|reason| {
            BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason,
            }
        })?;
        self.bytes.extend_from_slice(&child_bytes);
        self.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
        Ok(())
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
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !type_matches(&entry.ty, expected) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: requested_label,
            });
        }
        Ok((entry.offset, entry.size, entry.kind))
    }

    /// Locate a list-typed field by name, validating that the
    /// declared element type matches `expected_element`. Returns the
    /// pointer slot offset, the `FieldKind` for sanity-checking the
    /// pointer-indirect shape, and the [`ListElementKind`] sidecar.
    fn locate_list(
        &self,
        field_name: &str,
        expected_element: &TypeRepr,
        requested_label: &'static str,
    ) -> Result<(usize, FieldKind, ListElementKind), BufferError> {
        let entry = self
            .field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        let declared_elem = match &entry.ty {
            TypeRepr::List { element } => element.as_ref(),
            other => {
                return Err(BufferError::TypeMismatch {
                    name: field_name.to_string(),
                    declared: type_label(other),
                    requested: requested_label,
                });
            }
        };
        if !type_matches(declared_elem, expected_element) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: requested_label,
            });
        }
        let list_elem = entry
            .list_element
            .ok_or_else(|| BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "list field missing element layout",
            })?;
        Ok((entry.offset, entry.kind, list_elem))
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
        if let Some(target) = self.bytes.len().checked_next_multiple_of(align) {
            self.bytes.resize(target, 0);
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

/// Phase 10-c: in-flight `List<Schema>` element writer.
///
/// Buffered between [`BufferBuilder::list_record_writer`] and
/// [`BufferBuilder::finish_list_record`]. Each `start_entry` allocates
/// a detached child builder; `finish_entry` rebases the child's
/// internal pointers, appends the bytes to the parent's tail area,
/// and records the per-entry offset. The list header is written
/// only when the writer is finished, so the entry pointer array can
/// be filled with the final per-entry offsets without back-patches.
pub struct ListRecordWriter<'b> {
    field_name: String,
    slot_offset: usize,
    elem_layout: &'b OffsetTable,
    elem_schema: &'b Schema,
    entry_offsets: Vec<u32>,
    elem_align: usize,
}

impl<'b> ListRecordWriter<'b> {
    /// Allocate a fresh entry builder. The caller drives it with
    /// `write_int` / `write_string` / nested `sub_record` and hands
    /// it back via [`Self::finish_entry`].
    pub fn start_entry(&self) -> BufferBuilder<'b> {
        BufferBuilder::new(self.elem_layout, &self.elem_schema.fields)
    }

    /// Commit a previously-started entry into the parent buffer.
    ///
    /// Pads the parent tail area up to the schema's `root_align`,
    /// rebases the child's pointer-indirect slots through
    /// `relocate_pointers`, appends the bytes, and records the
    /// entry's offset for the eventual list header.
    pub fn finish_entry(
        &mut self,
        parent: &mut BufferBuilder<'_>,
        child: BufferBuilder<'_>,
    ) -> Result<(), BufferError> {
        if self.entry_offsets.len() >= u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: self.field_name.clone(),
                len: self.entry_offsets.len() + 1,
            });
        }
        let (child_bytes, child_reloc) = child.into_parts();
        let mut child_bytes = child_bytes;
        parent.pad_to(self.elem_align);
        let entry_offset = parent.bytes.len();
        let ptr = u32::try_from(entry_offset).map_err(|_| BufferError::ValueTooLarge {
            name: self.field_name.clone(),
            len: entry_offset,
        })?;
        relocate_pointers(&mut child_bytes, &child_reloc, 0, ptr).map_err(|reason| {
            BufferError::MalformedPayload {
                name: self.field_name.clone(),
                reason,
            }
        })?;
        parent.bytes.extend_from_slice(&child_bytes);
        self.entry_offsets.push(ptr);
        Ok(())
    }
}

/// Rebase every pointer-indirect slot inside `bytes` so the offsets
/// are valid once the whole buffer is pasted at `paste_base` of a
/// parent buffer. `record_base` is the byte offset of the record's
/// fixed area inside `bytes` (0 for the root call); recursion walks
/// nested `Schema` sub-records by following the slot's pre-relocation
/// value to find the inner fixed area.
///
/// All pointer slots in the input are expected to carry offsets
/// relative to `bytes`'s own base — i.e. the values a freshly built
/// child [`BufferBuilder`] would have written. After this routine each
/// slot is updated to `original_value + paste_base`, which matches the
/// parent buffer's coordinate system.
fn relocate_pointers(
    bytes: &mut [u8],
    reloc: &RelocLayout,
    record_base: usize,
    paste_base: u32,
) -> Result<(), &'static str> {
    for slot in &reloc.slots {
        let slot_abs = record_base
            .checked_add(slot.offset)
            .ok_or("pointer slot offset overflows usize")?;
        if slot_abs
            .checked_add(4)
            .map(|end| end > bytes.len())
            .unwrap_or(true)
        {
            return Err("pointer slot exceeds buffer end");
        }
        let mut ptr_buf = [0u8; 4];
        ptr_buf.copy_from_slice(&bytes[slot_abs..slot_abs + 4]);
        let original = u32::from_le_bytes(ptr_buf);
        let relocated = original
            .checked_add(paste_base)
            .ok_or("relocated pointer overflows u32")?;
        bytes[slot_abs..slot_abs + 4].copy_from_slice(&relocated.to_le_bytes());
        // For nested Schema fields, the pre-relocation pointer named
        // the inner record's fixed-area base inside `bytes`. Recurse
        // there so the inner record's own pointer-indirect slots get
        // rebased too — without recursion the wasm reader walking the
        // grand-child's String slot would fall off the parent buffer
        // by `paste_base` bytes.
        match slot.list_element {
            // Phase 10-c: `List<String>` / `List<Schema>` payloads are
            // pointer arrays whose entries also reference tail-area
            // records. Each entry needs `+ paste_base` so the reader can
            // still resolve through them. `List<Int>` / `List<Float>` /
            // `List<Bool>` are inline-fixed and need no per-element
            // rebase.
            Some(ListElementKind::PointerArray { .. }) => {
                relocate_list_pointer_array(
                    bytes,
                    original as usize,
                    slot.nested.as_deref(),
                    paste_base,
                )?;
            }
            Some(ListElementKind::InlineFixed { .. }) | None => {
                if let Some(nested) = slot.nested.as_deref() {
                    relocate_pointers(bytes, nested, original as usize, paste_base)?;
                }
            }
        }
    }
    Ok(())
}

/// Rebase every entry of a `List<String>` / `List<Schema>` pointer
/// array. `record_start` is the byte offset of the list's tail record
/// (the `[len: u32][off_0: u32]...` header). Walks the `len` entries,
/// adds `paste_base` to each, and — for Schema element types — recurses
/// into the per-element sub-record so its own pointer slots are
/// rebased too.
fn relocate_list_pointer_array(
    bytes: &mut [u8],
    record_start: usize,
    element_reloc: Option<&RelocLayout>,
    paste_base: u32,
) -> Result<(), &'static str> {
    if record_start
        .checked_add(4)
        .map(|end| end > bytes.len())
        .unwrap_or(true)
    {
        return Err("list length prefix exceeds buffer end");
    }
    let mut len_buf = [0u8; 4];
    len_buf.copy_from_slice(&bytes[record_start..record_start + 4]);
    let count = u32::from_le_bytes(len_buf) as usize;
    let mut cursor = record_start + 4;
    for _ in 0..count {
        if cursor
            .checked_add(4)
            .map(|end| end > bytes.len())
            .unwrap_or(true)
        {
            return Err("list pointer array entry exceeds buffer end");
        }
        let mut entry_buf = [0u8; 4];
        entry_buf.copy_from_slice(&bytes[cursor..cursor + 4]);
        let original = u32::from_le_bytes(entry_buf);
        let relocated = original
            .checked_add(paste_base)
            .ok_or("relocated list-entry pointer overflows u32")?;
        bytes[cursor..cursor + 4].copy_from_slice(&relocated.to_le_bytes());
        // Recurse into Schema entries so their own pointer-indirect
        // slots (e.g. an inner String) get rebased. `element_reloc` is
        // `None` for `List<String>` / `List<Int>` etc. where entries
        // don't carry further pointers.
        if let Some(element_reloc) = element_reloc {
            relocate_pointers(bytes, element_reloc, original as usize, paste_base)?;
        }
        cursor += 4;
    }
    Ok(())
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
    field_index: Vec<FieldEntry>,
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
                    .map(|f| FieldEntry {
                        name: fo.name.clone(),
                        ty: f.ty.clone(),
                        offset: fo.offset,
                        size: fo.size,
                        kind: fo.kind,
                        list_element: fo.list_element,
                    })
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

    /// Read a `List<Float>` from `field_name`. Tail layout mirrors
    /// `List<Int>` — `[len: u32][pad to 8][f64 elements]`.
    pub fn read_list_float(&self, field_name: &str) -> Result<Vec<f64>, BufferError> {
        let entry = self.find_entry(field_name)?;
        let elem = match &entry.ty {
            TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Float) => {
                element.as_ref()
            }
            _ => {
                return Err(BufferError::TypeMismatch {
                    name: field_name.to_string(),
                    declared: type_label(&entry.ty),
                    requested: "List<Float>",
                });
            }
        };
        let _ = elem;
        let FieldKind::PointerIndirect { tail_alignment } = entry.kind else {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        };
        let (count, payload_start) =
            self.decode_pointer_header(field_name, entry.offset, tail_alignment)?;
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
            out.push(f64::from_le_bytes(buf));
            cursor += 8;
        }
        Ok(out)
    }

    /// Read a `List<Bool>` from `field_name`. Tail layout `[len: u32]
    /// [u8 booleans]`. Non-zero bytes decode as `true` — defensive,
    /// the writer always emits `0` / `1`.
    pub fn read_list_bool(&self, field_name: &str) -> Result<Vec<bool>, BufferError> {
        let entry = self.find_entry(field_name)?;
        match &entry.ty {
            TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Bool) => {}
            _ => {
                return Err(BufferError::TypeMismatch {
                    name: field_name.to_string(),
                    declared: type_label(&entry.ty),
                    requested: "List<Bool>",
                });
            }
        }
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        }
        let (count, payload_start) = self.decode_pointer_header(field_name, entry.offset, 0)?;
        let payload_end =
            payload_start
                .checked_add(count)
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
        for i in 0..count {
            out.push(self.bytes[payload_start + i] != 0);
        }
        Ok(out)
    }

    /// Read a `List<String>` from `field_name`. Walks the pointer
    /// array, then decodes each per-entry `[len: u32][bytes]` record
    /// as UTF-8 borrowed from the underlying buffer.
    pub fn read_list_string(&self, field_name: &str) -> Result<Vec<&'a str>, BufferError> {
        let entry = self.find_entry(field_name)?;
        match &entry.ty {
            TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::String) => {}
            _ => {
                return Err(BufferError::TypeMismatch {
                    name: field_name.to_string(),
                    declared: type_label(&entry.ty),
                    requested: "List<String>",
                });
            }
        }
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        }
        // `decode_pointer_header` with `inner_alignment = 0` keeps the
        // payload start at `header + 4` (no extra pad), which is the
        // pointer-array start. Each entry is a u32 buffer-relative
        // offset pointing at a String `[len: u32][bytes]` record.
        let (count, entries_start) = self.decode_pointer_header(field_name, entry.offset, 0)?;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let cursor = entries_start + i * 4;
            if cursor + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list entry pointer exceeds buffer end",
                });
            }
            let mut entry_buf = [0u8; 4];
            entry_buf.copy_from_slice(&self.bytes[cursor..cursor + 4]);
            let record_start = u32::from_le_bytes(entry_buf) as usize;
            if record_start + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list string len prefix exceeds buffer end",
                });
            }
            let mut len_buf = [0u8; 4];
            len_buf.copy_from_slice(&self.bytes[record_start..record_start + 4]);
            let str_len = u32::from_le_bytes(len_buf) as usize;
            let payload_start = record_start + 4;
            let payload_end = payload_start.checked_add(str_len).ok_or_else(|| {
                BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list string payload end overflows usize",
                }
            })?;
            if payload_end > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list string payload exceeds buffer end",
                });
            }
            let s = std::str::from_utf8(&self.bytes[payload_start..payload_end]).map_err(|_| {
                BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list string payload is not valid utf-8",
                }
            })?;
            out.push(s);
        }
        Ok(out)
    }

    /// Read a `List<Schema>` from `field_name`. Returns a vector of
    /// [`BufferReader`]s, one per entry, each anchored at the
    /// matching sub-record's fixed area. The readers share the parent
    /// buffer slice so subsequent String / Int / Schema reads through
    /// them resolve back into the same tail area.
    pub fn read_list_record<'b>(
        &self,
        field_name: &str,
        elem_layout: &'b OffsetTable,
        elem_schema: &'b Schema,
    ) -> Result<Vec<BufferReader<'a>>, BufferError>
    where
        'b: 'a,
    {
        let entry = self.find_entry(field_name)?;
        match &entry.ty {
            TypeRepr::List { element } => match element.as_ref() {
                TypeRepr::Schema { schema } => {
                    if schema.as_ref() != elem_schema {
                        return Err(BufferError::TypeMismatch {
                            name: field_name.to_string(),
                            declared: type_label(&entry.ty),
                            requested: "List<Schema>",
                        });
                    }
                }
                _ => {
                    return Err(BufferError::TypeMismatch {
                        name: field_name.to_string(),
                        declared: type_label(&entry.ty),
                        requested: "List<Schema>",
                    });
                }
            },
            _ => {
                return Err(BufferError::TypeMismatch {
                    name: field_name.to_string(),
                    declared: type_label(&entry.ty),
                    requested: "List<Schema>",
                });
            }
        }
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        }
        let (count, entries_start) = self.decode_pointer_header(field_name, entry.offset, 0)?;
        let mut out: Vec<BufferReader<'a>> = Vec::with_capacity(count);
        for i in 0..count {
            let cursor = entries_start + i * 4;
            if cursor + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list entry pointer exceeds buffer end",
                });
            }
            let mut entry_buf = [0u8; 4];
            entry_buf.copy_from_slice(&self.bytes[cursor..cursor + 4]);
            let sub_base = u32::from_le_bytes(entry_buf) as usize;
            let sub_end = sub_base.checked_add(elem_layout.root_size).ok_or_else(|| {
                BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list sub-record end overflows usize",
                }
            })?;
            if sub_end > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "list sub-record exceeds buffer end",
                });
            }
            let field_index = elem_layout
                .fields
                .iter()
                .filter_map(|fo| {
                    elem_schema
                        .fields
                        .iter()
                        .find(|f| f.name == fo.name)
                        .map(|f| FieldEntry {
                            name: fo.name.clone(),
                            ty: f.ty.clone(),
                            offset: sub_base + fo.offset,
                            size: fo.size,
                            kind: fo.kind,
                            list_element: fo.list_element,
                        })
                })
                .collect();
            out.push(BufferReader {
                layout: elem_layout,
                field_index,
                bytes: self.bytes,
            });
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
    /// children (its String / `List<Int>` / nested Dict slots) keep
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
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !matches!(entry.ty, TypeRepr::Schema { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: "Schema",
            });
        }
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        }
        let ptr_offset = entry.offset;
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
        let sub_end = sub_base.checked_add(sub_layout.root_size).ok_or_else(|| {
            BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "sub-record end overflows usize",
            }
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
                sub_fields
                    .iter()
                    .find(|f| f.name == fo.name)
                    .map(|f| FieldEntry {
                        name: fo.name.clone(),
                        ty: f.ty.clone(),
                        offset: sub_base + fo.offset,
                        size: fo.size,
                        kind: fo.kind,
                        list_element: fo.list_element,
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
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        if !type_matches(&entry.ty, expected) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: requested_label,
            });
        }
        Ok((entry.offset, entry.size, entry.kind))
    }

    /// Find an entry by name. Used by the list readers when they need
    /// the carried `list_element` sidecar alongside the offset.
    fn find_entry(&self, field_name: &str) -> Result<&FieldEntry, BufferError> {
        self.field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })
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
            match (d.as_ref(), r.as_ref()) {
                (TypeRepr::Int, TypeRepr::Int)
                | (TypeRepr::Float, TypeRepr::Float)
                | (TypeRepr::Bool, TypeRepr::Bool)
                | (TypeRepr::String, TypeRepr::String) => true,
                (TypeRepr::Schema { schema: ds }, TypeRepr::Schema { schema: rs }) => ds == rs,
                _ => false,
            }
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
        TypeRepr::Closure { .. } => "Closure",
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

    /// F-5 wire-format smoke gate: pin the exact tail-record bytes a
    /// `write_string` call emits. The buffer-protocol producer (this
    /// fn) and every consumer (cranelift `emit_read_string_len`,
    /// stdlib `string_*` bodies indexing payload at `s + 4`,
    /// `decode_pointer_header` in `read_string`) all hard-code the
    /// `[len:u32 LE][payload]` shape. Flipping to the 12-byte
    /// `[len_with_ascii_flag:u32 LE][hash:u64 LE][payload]` planned
    /// by `docs/internal/review-improvement-169-conststring-wire-full-2026-05-22.md`
    /// must update producer + every consumer atomically; this test
    /// fires the moment the producer side drifts so the migrant cannot
    /// silently land a partial revision.
    ///
    /// See also `relon-codegen-native::codegen::const_pool::
    /// opvisitor_emits_const_string_record_in_declaration_order` for
    /// the matching pin on the cranelift const-pool producer.
    #[test]
    fn write_string_wire_format_smoke_gate() {
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_string("name", "hello").expect("write name");
        let bytes = builder.finish();

        // Fixed-area: a 4-byte pointer slot at offset 0 carrying the
        // tail-record absolute offset. The schema has no other fields,
        // so root_size = 4, root_align = 4 -> tail starts at offset 4.
        assert_eq!(bytes.len(), 4 + 4 + 5, "fixed-area + header + payload");
        let ptr = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        assert_eq!(ptr, 4, "pointer slot must reference offset 4");

        // Tail record at offset 4: `[len: u32 LE][payload]`. Migration
        // to the 12-byte header changes this to
        // `[len_with_ascii_flag: u32 LE][hash: u64 LE][payload]`.
        assert_eq!(
            &bytes[4..8],
            &5u32.to_le_bytes(),
            "tail record len prefix must be u32 LE of payload length"
        );
        assert_eq!(&bytes[8..13], b"hello", "payload follows the 4-byte header");
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
    fn write_sub_record_simple_schema_arg_roundtrips() {
        // `Wrap { User u }` where User { Int age } — writer fills the
        // parent's pointer slot via `sub_record`, reader walks back via
        // `BufferReader::sub_record` and reads the inner `age`.
        let user_schema = Schema {
            name: "User".into(),
            generics: vec![],
            fields: vec![field("age", TypeRepr::Int)],
        };
        let wrap_schema = Schema {
            name: "Wrap".into(),
            generics: vec![],
            fields: vec![field(
                "u",
                TypeRepr::Schema {
                    schema: Box::new(user_schema.clone()),
                },
            )],
        };
        let wrap_layout = SchemaLayout::offsets_for(&wrap_schema).expect("wrap layout");
        let user_layout = SchemaLayout::offsets_for(&user_schema).expect("user layout");

        let mut wrap_builder = BufferBuilder::new(&wrap_layout, &wrap_schema.fields);
        let mut user_builder = wrap_builder
            .sub_record("u", &user_layout, &user_schema.fields)
            .expect("sub_record");
        user_builder.write_int("age", 42).expect("write age");
        wrap_builder
            .finish_sub_record("u", user_builder)
            .expect("finish_sub_record");
        let bytes = wrap_builder.finish();

        let reader = BufferReader::new(&wrap_layout, &wrap_schema.fields, &bytes).expect("reader");
        let sub = reader
            .sub_record("u", &user_layout, &user_schema.fields)
            .expect("sub");
        assert_eq!(sub.read_int("age").expect("read age"), 42);
    }

    #[test]
    fn write_sub_record_nested_with_string_field() {
        // `Usr { Addr addr, String name }` — exercises both the
        // sub_record writer and the surrounding parent-area String slot
        // sharing the same tail area.
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

        let mut usr_builder = BufferBuilder::new(&usr_layout, &usr_schema.fields);
        let mut addr_builder = usr_builder
            .sub_record("addr", &addr_layout, &addr_schema.fields)
            .expect("sub_record");
        addr_builder.write_string("city", "BJ").expect("write city");
        addr_builder.write_int("zip", 100000).expect("write zip");
        usr_builder
            .finish_sub_record("addr", addr_builder)
            .expect("finish_sub_record");
        usr_builder.write_string("name", "Bob").expect("write name");
        let bytes = usr_builder.finish();

        let reader = BufferReader::new(&usr_layout, &usr_schema.fields, &bytes).expect("reader");
        let sub = reader
            .sub_record("addr", &addr_layout, &addr_schema.fields)
            .expect("sub");
        assert_eq!(sub.read_string("city").expect("city"), "BJ");
        assert_eq!(sub.read_int("zip").expect("zip"), 100000);
        assert_eq!(reader.read_string("name").expect("name"), "Bob");
    }

    #[test]
    fn write_sub_record_inner_list_int_roundtrips() {
        // Mixed Schema arg: parent has the sub-record + a top-level
        // List<Int>; the sub-record itself has a String. Confirms the
        // tail-area cursor advances correctly across multiple
        // heterogenous appends.
        let inner_schema = Schema {
            name: "Inner".into(),
            generics: vec![],
            fields: vec![field("tag", TypeRepr::String)],
        };
        let outer_schema = Schema {
            name: "Outer".into(),
            generics: vec![],
            fields: vec![
                field(
                    "child",
                    TypeRepr::Schema {
                        schema: Box::new(inner_schema.clone()),
                    },
                ),
                field(
                    "nums",
                    TypeRepr::List {
                        element: Box::new(TypeRepr::Int),
                    },
                ),
            ],
        };
        let outer_layout = SchemaLayout::offsets_for(&outer_schema).expect("outer layout");
        let inner_layout = SchemaLayout::offsets_for(&inner_schema).expect("inner layout");

        let mut outer = BufferBuilder::new(&outer_layout, &outer_schema.fields);
        let mut inner = outer
            .sub_record("child", &inner_layout, &inner_schema.fields)
            .expect("sub_record");
        inner.write_string("tag", "hello").expect("write tag");
        outer
            .finish_sub_record("child", inner)
            .expect("finish_sub_record");
        outer
            .write_list_int("nums", &[10, 20, 30])
            .expect("write nums");
        let bytes = outer.finish();

        let reader =
            BufferReader::new(&outer_layout, &outer_schema.fields, &bytes).expect("reader");
        let sub = reader
            .sub_record("child", &inner_layout, &inner_schema.fields)
            .expect("sub");
        assert_eq!(sub.read_string("tag").expect("tag"), "hello");
        assert_eq!(
            reader.read_list_int("nums").expect("nums"),
            vec![10, 20, 30]
        );
    }

    #[test]
    fn write_sub_record_unknown_field_rejected() {
        let inner_schema = Schema {
            name: "Inner".into(),
            generics: vec![],
            fields: vec![field("x", TypeRepr::Int)],
        };
        let outer_schema = Schema {
            name: "Outer".into(),
            generics: vec![],
            fields: vec![field(
                "child",
                TypeRepr::Schema {
                    schema: Box::new(inner_schema.clone()),
                },
            )],
        };
        let outer_layout = SchemaLayout::offsets_for(&outer_schema).expect("outer layout");
        let inner_layout = SchemaLayout::offsets_for(&inner_schema).expect("inner layout");
        let mut outer = BufferBuilder::new(&outer_layout, &outer_schema.fields);
        let err = outer
            .sub_record("missing", &inner_layout, &inner_schema.fields)
            .expect_err("missing field must reject");
        assert!(matches!(err, BufferError::UnknownField { ref name } if name == "missing"));
    }

    #[test]
    fn write_sub_record_on_non_schema_slot_rejected() {
        // `name` is a String — sub_record must refuse rather than
        // patch a 4-byte pointer into a slot the layout reserved for
        // a String tail-record offset.
        let inner_schema = Schema {
            name: "Inner".into(),
            generics: vec![],
            fields: vec![field("x", TypeRepr::Int)],
        };
        let mixed = Schema {
            name: "Mixed".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let mixed_layout = SchemaLayout::offsets_for(&mixed).expect("mixed layout");
        let inner_layout = SchemaLayout::offsets_for(&inner_schema).expect("inner layout");
        let mut builder = BufferBuilder::new(&mixed_layout, &mixed.fields);
        let err = builder
            .sub_record("name", &inner_layout, &inner_schema.fields)
            .expect_err("non-schema slot must reject");
        assert!(matches!(
            err,
            BufferError::TypeMismatch {
                requested: "Schema",
                ..
            }
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

    // =================================================================
    // Phase 10-c: List<Float / Bool / String / Schema> roundtrips.
    // =================================================================

    fn list_schema(name: &str, elem: TypeRepr) -> Schema {
        Schema {
            name: "Wrap".into(),
            generics: vec![],
            fields: vec![field(
                name,
                TypeRepr::List {
                    element: Box::new(elem),
                },
            )],
        }
    }

    #[test]
    fn write_list_float_then_read_back_roundtrips() {
        let schema = list_schema("xs", TypeRepr::Float);
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_float("xs", &[1.5, -2.25, 3.125])
            .expect("write");
        let bytes = b.finish();
        let r = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(
            r.read_list_float("xs").expect("read"),
            vec![1.5, -2.25, 3.125]
        );
    }

    #[test]
    fn write_list_bool_then_read_back_roundtrips() {
        let schema = list_schema("xs", TypeRepr::Bool);
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_bool("xs", &[true, false, true, true])
            .expect("write");
        let bytes = b.finish();
        let r = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(
            r.read_list_bool("xs").expect("read"),
            vec![true, false, true, true]
        );
    }

    #[test]
    fn empty_list_bool_roundtrips() {
        // Zero-element payload still needs a valid `[len=0]` prefix
        // the reader can walk through without OOB checks tripping.
        let schema = list_schema("xs", TypeRepr::Bool);
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_bool("xs", &[]).expect("write");
        let bytes = b.finish();
        let r = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert!(r.read_list_bool("xs").expect("read").is_empty());
    }

    #[test]
    fn write_list_string_then_read_back_roundtrips() {
        let schema = list_schema("xs", TypeRepr::String);
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_string("xs", &["alpha", "beta", "", "gamma"])
            .expect("write");
        let bytes = b.finish();
        let r = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(
            r.read_list_string("xs").expect("read"),
            vec!["alpha", "beta", "", "gamma"]
        );
    }

    #[test]
    fn empty_list_string_roundtrips() {
        let schema = list_schema("xs", TypeRepr::String);
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_string::<&str>("xs", &[]).expect("write");
        let bytes = b.finish();
        let r = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert!(r.read_list_string("xs").expect("read").is_empty());
    }

    #[test]
    fn mixed_record_and_list_string_roundtrip() {
        // Phase 10-c: a top-level Int + a List<String> share the tail
        // area. Verifies the cursor advances correctly across heterogeneous
        // tail appends.
        let schema = Schema {
            name: "Mixed".into(),
            generics: vec![],
            fields: vec![
                field("n", TypeRepr::Int),
                field(
                    "xs",
                    TypeRepr::List {
                        element: Box::new(TypeRepr::String),
                    },
                ),
                field("name", TypeRepr::String),
            ],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_int("n", 7).expect("write n");
        b.write_list_string("xs", &["ada", "bob"])
            .expect("write xs");
        b.write_string("name", "ada").expect("write name");
        let bytes = b.finish();
        let r = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(r.read_int("n").expect("n"), 7);
        assert_eq!(r.read_list_string("xs").expect("xs"), vec!["ada", "bob"]);
        assert_eq!(r.read_string("name").expect("name"), "ada");
    }

    #[test]
    fn write_list_record_with_nested_string_roundtrip() {
        // `List<User>` where User { String name, Int age }. Verifies
        // both the per-entry pointer array and the inner string
        // payload's relocation through the parent buffer.
        let user_schema = Schema {
            name: "User".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String), field("age", TypeRepr::Int)],
        };
        let outer = Schema {
            name: "Group".into(),
            generics: vec![],
            fields: vec![field(
                "users",
                TypeRepr::List {
                    element: Box::new(TypeRepr::Schema {
                        schema: Box::new(user_schema.clone()),
                    }),
                },
            )],
        };
        let outer_layout = SchemaLayout::offsets_for(&outer).expect("outer layout");
        let user_layout = SchemaLayout::offsets_for(&user_schema).expect("user layout");

        let mut b = BufferBuilder::new(&outer_layout, &outer.fields);
        let users_data: Vec<(&str, i64)> = vec![("ada", 36), ("bob", 41), ("zoe", 19)];
        let mut writer = b
            .list_record_writer("users", &user_layout, &user_schema)
            .expect("list_record_writer");
        for (name, age) in &users_data {
            let mut child = writer.start_entry();
            child.write_string("name", name).expect("write name");
            child.write_int("age", *age).expect("write age");
            writer.finish_entry(&mut b, child).expect("finish entry");
        }
        b.finish_list_record(writer).expect("finish list");
        let bytes = b.finish();

        let r = BufferReader::new(&outer_layout, &outer.fields, &bytes).expect("reader");
        let entries = r
            .read_list_record("users", &user_layout, &user_schema)
            .expect("read list");
        assert_eq!(entries.len(), 3);
        for (sub, (name, age)) in entries.iter().zip(users_data.iter()) {
            assert_eq!(sub.read_string("name").expect("name"), *name);
            assert_eq!(sub.read_int("age").expect("age"), *age);
        }
    }

    #[test]
    fn list_string_type_mismatch_rejected() {
        let schema = list_schema("xs", TypeRepr::Int);
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        let err = b
            .write_list_string("xs", &["nope"])
            .expect_err("must reject");
        assert!(matches!(
            err,
            BufferError::TypeMismatch {
                requested: "List<String>",
                ..
            }
        ));
    }
}
