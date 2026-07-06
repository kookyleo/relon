//! Typesafe writer + reader for the host <-> wasm binary handshake.
//!
//! Spec: `docs/internal/adr/wasm-binary-layout-v1-2026-05-16.md`.
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
    /// For a `PointerArray` list slot, the recursive descriptor of what
    /// each entry points at — so the relocation walker can rebase the
    /// entries' own internal pointers (`List<Schema>` sub-records,
    /// `List<List<String|Schema>>` inner pointer-array lists). `None` for
    /// inline-fixed / scalar element lists whose entries carry no further
    /// pointers. This is the F5 piece: it lets a doubly-nested
    /// pointer-array list (`List<List<String>>`) be relocated to one level
    /// deeper than the `nested` schema cache alone could reach.
    list_elem: Option<PtrArrayElem>,
    /// For a direct `Option<T>` / `Result<T, E>` pointer slot, the variant
    /// type whose selected payload may itself contain pointer slots that must
    /// be relocated after a paste / arena rebase.
    variant: Option<TypeRepr>,
}

/// Recursive descriptor of one `PointerArray` list level's element, used
/// by the relocation walker to rebase the pointers an entry introduces.
/// Built once from the field's declared [`TypeRepr`] at builder
/// construction; mirrors the depth the verifier / reader recurse to.
#[derive(Debug)]
pub(crate) enum PtrArrayElem {
    /// `List<String>` elements: each entry points at a `[len][utf8]`
    /// String record with no further pointers — entry-pointer rebase only.
    String,
    /// `List<Schema>` elements: each entry points at a sub-record whose
    /// own pointer slots are rebased via the cached [`RelocLayout`].
    Schema(Arc<RelocLayout>),
    /// `List<List<…>>` elements: each entry points at an **inner list
    /// header** that is itself a pointer array. Recurse one level: rebase
    /// the inner header's entries (and, for the inner element, whatever
    /// `inner` describes) too. This is what lifts the `List<List<String>>`
    /// / `List<List<Schema>>` cap — the inner pointer array's entries would
    /// otherwise keep their child-buffer-relative offsets and dereference
    /// `paste_base` bytes off.
    InnerList {
        /// Element descriptor of the inner list (`String` / `Schema` /
        /// a still-deeper `InnerList`).
        inner: Box<PtrArrayElem>,
    },
    /// `List<Option<T>>` / `List<Result<T, E>>` elements: each entry points
    /// at a variant record whose selected payload may recursively contain
    /// pointers.
    Variant(TypeRepr),
}

/// Build the recursive [`PtrArrayElem`] descriptor for a pointer-array
/// list whose declared element type is `element`. Returns `None` for an
/// inline-fixed element (`Int` / `Float` / `Bool`) where no per-entry
/// recursion is needed — the entry-pointer rebase the walker always does
/// suffices.
fn ptr_array_elem_for(element: &TypeRepr) -> Option<PtrArrayElem> {
    match element {
        TypeRepr::String => Some(PtrArrayElem::String),
        TypeRepr::Schema { schema } => SchemaLayout::offsets_for(schema)
            .ok()
            .map(|sub| PtrArrayElem::Schema(RelocLayout::build(&sub, &schema.fields))),
        // A `List<inner>` element. Only recurse when the inner list is
        // itself a **pointer array** carrying internal pointers
        // (`List<List<String|Schema|List>>`): then each entry points at an
        // inner pointer-array header whose own entries must be rebased. An
        // inner *inline-fixed* scalar list (`List<List<Int>>`) is a
        // self-contained `[len][payload]` record with no internal pointers,
        // so the outer entry-pointer rebase is all that is needed —
        // recursing into it would mis-treat the i64 payload as entry
        // pointers and corrupt the buffer.
        TypeRepr::List { element: inner } => {
            ptr_array_elem_for(inner).map(|inner_desc| PtrArrayElem::InnerList {
                inner: Box::new(inner_desc),
            })
        }
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            Some(PtrArrayElem::Variant(element.clone()))
        }
        // Inline-fixed scalar element: entry-pointer rebase only.
        _ => None,
    }
}

/// The strongest byte alignment any value of declared type `ty` requires
/// **anywhere in its serialised graph** — fixed area *and* tail records.
///
/// The tail-area scalar-list writers
/// ([`BufferBuilder::append_tail_record_with_inner_alignment`]) place an
/// `Int` / `Float` payload on an 8-byte boundary, computed in the
/// authoring buffer's coordinates. When a sub-record / list entry is later
/// pasted into a parent and its pointers are relocated by adding the paste
/// base, the payload bytes are **not** moved — so the reader's absolute
/// `(rec_start + 4).next_multiple_of(8)` recovery only lands on the bytes
/// the writer wrote when the paste base is itself a multiple of 8. A
/// record's fixed-area `root_align` does not capture this (the 8-aligned
/// content lives in the tail), so paste sites must additionally honour the
/// **deep tail alignment** computed here. Returns 1 when nothing inside
/// `ty` needs more than byte alignment.
fn type_graph_align(ty: &TypeRepr) -> usize {
    match ty {
        // Inline-fixed scalar / String tail (`[len][utf8]`) — 4 suffices;
        // the record-prefix `pad_to(4)` already covers them.
        TypeRepr::Unit | TypeRepr::Bool | TypeRepr::Int | TypeRepr::Float | TypeRepr::String => 1,
        TypeRepr::List { element } => match element.as_ref() {
            // `List<Int>` / `List<Float>` tail payload sits on an 8-byte
            // boundary; `List<Bool>` packs at 1.
            TypeRepr::Int | TypeRepr::Float => 8,
            TypeRepr::Bool => 1,
            // String element pointer array — 4-aligned offsets only.
            TypeRepr::String => 1,
            // Recurse into the element graph (`List<List<Int>>`,
            // `List<Schema>`, deeper nests).
            other => type_graph_align(other),
        },
        TypeRepr::Schema { schema } => schema
            .fields
            .iter()
            .map(|f| type_graph_align(&f.ty))
            .max()
            .unwrap_or(1),
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            variant_record_align_runtime(ty)
        }
        // Closure values never reach the host-visible buffer protocol.
        TypeRepr::Closure { .. } => 1,
    }
}

/// The deepest paste alignment a schema's serialised graph requires:
/// `max(root_align, deep tail alignment of every field)`. Used at every
/// sub-record / list-entry paste so an 8-aligned `List<Int|Float>` (or a
/// nested-list / nested-schema field that transitively carries one) in the
/// tail lands at an absolute offset where the reader's alignment recovery
/// is correct after relocation.
fn schema_paste_align(layout: &OffsetTable, fields: &[Field]) -> usize {
    let tail = fields
        .iter()
        .map(|f| type_graph_align(&f.ty))
        .max()
        .unwrap_or(1);
    layout.root_align.max(tail).max(1)
}

fn payload_slot_layout_runtime(ty: &TypeRepr) -> (usize, usize) {
    match ty {
        TypeRepr::Unit | TypeRepr::Bool => (1, 1),
        TypeRepr::Int | TypeRepr::Float => (8, 8),
        TypeRepr::String
        | TypeRepr::List { .. }
        | TypeRepr::Schema { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Enum { .. }
        | TypeRepr::Closure { .. } => (4, 4),
    }
}

#[derive(Debug, Clone)]
struct SelectedVariant {
    name: String,
    payload: Option<SelectedVariantPayload>,
}

#[derive(Debug, Clone)]
struct SelectedVariantPayload {
    ty: TypeRepr,
    /// `Some(key)` for scalar-like single-field wrappers (`Option.Some`,
    /// `Result.Ok`, `Result.Err`). `None` means the payload type is a schema
    /// whose decoded field map is used directly for a custom enum variant.
    key: Option<&'static str>,
}

fn variant_payload_types(ty: &TypeRepr) -> Vec<TypeRepr> {
    match ty {
        TypeRepr::Option { inner } => vec![inner.as_ref().clone()],
        TypeRepr::Result { ok, err } => vec![ok.as_ref().clone(), err.as_ref().clone()],
        TypeRepr::Enum { name, variants } => variants
            .iter()
            .filter_map(|variant| {
                variant.payload_schema(name).map(|schema| TypeRepr::Schema {
                    schema: Box::new(schema),
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn variant_record_align_runtime(ty: &TypeRepr) -> usize {
    let mut align = 4usize;
    for payload in variant_payload_types(ty) {
        let (_, slot_align) = payload_slot_layout_runtime(&payload);
        align = align.max(slot_align).max(type_graph_align(&payload));
    }
    align
}

fn variant_payload_slot_offset(record_start: usize, payload_ty: &TypeRepr) -> Option<usize> {
    let (_, align) = payload_slot_layout_runtime(payload_ty);
    let raw = record_start.checked_add(1)?;
    if align <= 1 {
        Some(raw)
    } else {
        raw.checked_next_multiple_of(align)
    }
}

fn variant_selected_payload(ty: &TypeRepr, tag: u8) -> Result<SelectedVariant, &'static str> {
    match ty {
        TypeRepr::Option { inner } => match tag {
            0 => Ok(SelectedVariant {
                name: "None".to_string(),
                payload: None,
            }),
            1 => Ok(SelectedVariant {
                name: "Some".to_string(),
                payload: Some(SelectedVariantPayload {
                    ty: inner.as_ref().clone(),
                    key: Some("value"),
                }),
            }),
            _ => Err("invalid Option tag"),
        },
        TypeRepr::Result { ok, err } => match tag {
            0 => Ok(SelectedVariant {
                name: "Ok".to_string(),
                payload: Some(SelectedVariantPayload {
                    ty: ok.as_ref().clone(),
                    key: Some("value"),
                }),
            }),
            1 => Ok(SelectedVariant {
                name: "Err".to_string(),
                payload: Some(SelectedVariantPayload {
                    ty: err.as_ref().clone(),
                    key: Some("error"),
                }),
            }),
            _ => Err("invalid Result tag"),
        },
        TypeRepr::Enum { name, variants } => {
            let variant = variants
                .iter()
                .find(|variant| variant.tag == tag)
                .ok_or("invalid enum tag")?;
            Ok(SelectedVariant {
                name: variant.name.clone(),
                payload: variant
                    .payload_schema(name)
                    .map(|schema| SelectedVariantPayload {
                        ty: TypeRepr::Schema {
                            schema: Box::new(schema),
                        },
                        key: None,
                    }),
            })
        }
        _ => Err("expected variant record"),
    }
}

fn list_payload_is_pointer_array(element: &TypeRepr) -> bool {
    matches!(
        element,
        TypeRepr::String
            | TypeRepr::Schema { .. }
            | TypeRepr::List { .. }
            | TypeRepr::Option { .. }
            | TypeRepr::Result { .. }
            | TypeRepr::Enum { .. }
    )
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
            // For a list field, pre-compute the recursive element
            // descriptor so the relocation walker can rebase pointers one
            // (or more) level deeper than `nested` alone reaches — the
            // `List<List<String|Schema>>` inner pointer arrays.
            let list_elem = declared.and_then(|f| match &f.ty {
                TypeRepr::List { element } => ptr_array_elem_for(element),
                _ => None,
            });
            let variant = declared.and_then(|f| match &f.ty {
                TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
                    Some(f.ty.clone())
                }
                _ => None,
            });
            slots.push(RelocSlot {
                offset: fo.offset,
                list_element: fo.list_element,
                nested,
                list_elem,
                variant,
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

    /// Mark `field_name` as an internal unit slot. The slot is already zeroed by
    /// `new`, so this is a no-op beyond the type check — useful to
    /// surface a `TypeMismatch` early when the host accidentally
    /// writes a unit marker to a non-unit slot.
    pub fn write_unit(&mut self, field_name: &str) -> Result<(), BufferError> {
        let (_, _, _) = self.locate(field_name, &TypeRepr::Unit, "Unit")?;
        Ok(())
    }

    /// Write any supported value shape into `field_name` using the declared
    /// canonical type. This is the generic marshalling entry point used by
    /// nested schemas, tuples, and the compiled backends for types that do not
    /// have a dedicated `write_*` convenience method.
    pub fn write_value(
        &mut self,
        field_name: &str,
        ty: &TypeRepr,
        value: &relon_eval_api::value::Value,
    ) -> Result<(), BufferError> {
        let entry = self.find_entry(field_name)?.clone();
        if !type_matches(&entry.ty, ty) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: type_label(ty),
            });
        }
        self.write_value_slot(field_name, entry.offset, ty, value)
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
        self.bytes.extend_from_slice(
            &u32::try_from(count)
                .expect("count already checked <= u32::MAX")
                .to_le_bytes(),
        );
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

    /// Write a nested `List<List<inner>>` into `field_name`'s tail
    /// area, where `inner` is an inline-fixed scalar element
    /// (`Int` / `Float` / `Bool`).
    ///
    /// Layout mirrors `List<String>` / `List<Schema>`: a header
    /// `[len: u32 LE][off_0]...[off_(n-1)]` whose `off_i` are
    /// buffer-relative offsets to per-element inner list records. Each
    /// inner record is the same `[len: u32 LE][payload]` shape
    /// [`Self::write_list_int`] / `write_list_float` / `write_list_bool`
    /// produce, so an inner-record reader decodes them bit-identically.
    /// The inner records carry no pointer slots of their own, so no
    /// per-element relocation beyond the header's `off_i` rebase is
    /// needed when the buffer is later pasted into a parent.
    ///
    /// `encode_inner` serialises one element's payload bytes and returns
    /// `(element_count, inner_alignment)`; the caller drives it once per
    /// inner list. Returns the count actually written.
    pub fn write_list_list_with<F>(
        &mut self,
        field_name: &str,
        inner_count: usize,
        mut encode_inner: F,
    ) -> Result<(), BufferError>
    where
        F: FnMut(usize, &mut Vec<u8>) -> Result<(usize, usize), BufferError>,
    {
        let entry = self
            .field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        let is_nested_list = matches!(&entry.ty, TypeRepr::List { element }
            if matches!(element.as_ref(), TypeRepr::List { .. }));
        if !is_nested_list || !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: "List<List<…>>",
            });
        }
        let slot_offset = entry.offset;
        if inner_count > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: field_name.to_string(),
                len: inner_count,
            });
        }
        // Reserve the header `[len][off_0]...` first; back-patch each
        // `off_i` after the matching inner record lands.
        self.pad_to(4);
        let header_offset = self.bytes.len();
        self.bytes
            .extend_from_slice(&u32::try_from(inner_count).unwrap().to_le_bytes());
        let entries_start = self.bytes.len();
        self.bytes.resize(entries_start + inner_count * 4, 0);
        let mut offsets: Vec<u32> = Vec::with_capacity(inner_count);
        for i in 0..inner_count {
            let mut payload = Vec::new();
            let (elem_count, inner_alignment) = encode_inner(i, &mut payload)?;
            let rec_offset = self.append_tail_record_with_inner_alignment(
                4,
                inner_alignment.max(1),
                elem_count,
                &payload,
            );
            let rec_u32 = u32::try_from(rec_offset).map_err(|_| BufferError::ValueTooLarge {
                name: format!("{field_name}[{i}]"),
                len: rec_offset,
            })?;
            offsets.push(rec_u32);
        }
        for (i, off) in offsets.iter().enumerate() {
            let dst = entries_start + i * 4;
            self.bytes[dst..dst + 4].copy_from_slice(&off.to_le_bytes());
        }
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
            // Paste each entry at the element schema's deep paste alignment
            // (not just its fixed-area `root_align`) so an 8-aligned
            // `List<Int|Float>` payload in the entry's tail lands where the
            // reader's absolute alignment recovery expects it post-paste.
            elem_align: schema_paste_align(elem_layout, &elem_schema.fields),
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

    /// Consume the builder and return the byte buffer with every
    /// pointer slot rebased from **buffer-relative** to
    /// **arena-absolute** by adding `arena_base` (the absolute arena
    /// offset the buffer is about to be copied to — i.e. `in_ptr`).
    ///
    /// F1 unifies the in-buffer pointer convention on a single
    /// arena-absolute basis: the input marshaller knows `in_ptr` at this
    /// point, so it bakes it into every slot here once, and the machine
    /// code's param-read drops its old `+ in_ptr` rebase (the slot is
    /// already arena-absolute). The same recursive walk
    /// [`finish_sub_record`] uses to paste a child into a parent applies
    /// — a rebase by `arena_base` is structurally identical to a paste at
    /// `arena_base`, relocating every nested-schema and pointer-array
    /// entry too. `arena_base == 0` is a no-op (the slots are already
    /// correct), so a zero-const-data layout stays byte-identical.
    pub fn finish_arena_absolute(self, arena_base: u32) -> Result<Vec<u8>, BufferError> {
        let (mut bytes, reloc) = self.into_parts();
        if arena_base != 0 {
            relocate_pointers(&mut bytes, &reloc, 0, arena_base).map_err(|reason| {
                BufferError::MalformedPayload {
                    name: "<input marshal arena rebase>".to_string(),
                    reason,
                }
            })?;
        }
        Ok(bytes)
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
        // Paste at the child's deep paste alignment so an 8-aligned
        // `List<Int|Float>` (or a nested field transitively carrying one)
        // in the child's tail lands at an absolute offset where the
        // reader's alignment recovery is correct after relocation — not
        // just the child's fixed-area `root_align`.
        let child_tail_align = child
            .field_index
            .iter()
            .map(|fe| type_graph_align(&fe.ty))
            .max()
            .unwrap_or(1);
        let child_align = child
            .layout
            .root_align
            .max(tail_alignment)
            .max(child_tail_align)
            .max(1);
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

    fn find_entry(&self, field_name: &str) -> Result<&FieldEntry, BufferError> {
        self.field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })
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

    fn write_value_slot(
        &mut self,
        name: &str,
        slot_offset: usize,
        ty: &TypeRepr,
        value: &relon_eval_api::value::Value,
    ) -> Result<(), BufferError> {
        use relon_eval_api::value::Value;
        match (ty, value) {
            (TypeRepr::Int, Value::Int(v)) => {
                self.bytes[slot_offset..slot_offset + 8].copy_from_slice(&v.to_le_bytes());
                Ok(())
            }
            (TypeRepr::Float, Value::Float(v)) => {
                self.bytes[slot_offset..slot_offset + 8]
                    .copy_from_slice(&v.into_inner().to_le_bytes());
                Ok(())
            }
            (TypeRepr::Float, Value::Int(v)) => {
                self.bytes[slot_offset..slot_offset + 8]
                    .copy_from_slice(&(*v as f64).to_le_bytes());
                Ok(())
            }
            (TypeRepr::Bool, Value::Bool(v)) => {
                self.bytes[slot_offset] = u8::from(*v);
                Ok(())
            }
            (TypeRepr::Unit, v) if v.is_option_none() => Ok(()),
            (TypeRepr::String, Value::String(s)) => {
                let off = self.append_tail_record(4, s.len(), s.as_str().as_bytes());
                self.write_pointer_slot(name, slot_offset, off)
            }
            (TypeRepr::List { element }, Value::List(items)) => {
                let off = self.append_list_payload(element, items)?;
                self.write_pointer_slot(name, slot_offset, off)
            }
            (TypeRepr::Schema { schema }, v) => {
                let off = self.append_schema_value_payload(name, schema, v)?;
                self.write_pointer_slot(name, slot_offset, off)
            }
            (TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. }, v) => {
                let off = self.append_variant_record(name, ty, v)?;
                self.write_pointer_slot(name, slot_offset, off)
            }
            (_, other) => Err(BufferError::TypeMismatch {
                name: name.to_string(),
                declared: type_label(ty),
                requested: other.type_name(),
            }),
        }
    }

    fn write_pointer_slot(
        &mut self,
        name: &str,
        slot_offset: usize,
        target_offset: usize,
    ) -> Result<(), BufferError> {
        let ptr = u32::try_from(target_offset).map_err(|_| BufferError::ValueTooLarge {
            name: name.to_string(),
            len: target_offset,
        })?;
        self.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
        Ok(())
    }

    fn append_schema_value_payload(
        &mut self,
        name: &str,
        schema: &Schema,
        value: &relon_eval_api::value::Value,
    ) -> Result<usize, BufferError> {
        use relon_eval_api::value::Value;
        let sub_layout =
            SchemaLayout::offsets_for(schema).map_err(|_| BufferError::MalformedPayload {
                name: name.to_string(),
                reason: "nested schema field is not layoutable",
            })?;
        let mut child = BufferBuilder::new(&sub_layout, &schema.fields);
        match value {
            Value::Dict(dict) if !schema.is_tuple => {
                write_schema_record_into_builder(&mut child, schema, dict)?;
            }
            Value::Tuple(items) if schema.is_tuple => {
                write_tuple_record_into_builder(&mut child, schema, items)?;
            }
            other => {
                return Err(BufferError::TypeMismatch {
                    name: name.to_string(),
                    declared: if schema.is_tuple { "Tuple" } else { "Schema" },
                    requested: other.type_name(),
                })
            }
        }
        let align = schema_paste_align(&sub_layout, &schema.fields);
        self.append_child_record_payload(name, child, align)
    }

    fn append_child_record_payload(
        &mut self,
        name: &str,
        child: BufferBuilder<'_>,
        requested_align: usize,
    ) -> Result<usize, BufferError> {
        let child_tail_align = child
            .field_index
            .iter()
            .map(|fe| type_graph_align(&fe.ty))
            .max()
            .unwrap_or(1);
        let child_align = child
            .layout
            .root_align
            .max(requested_align)
            .max(child_tail_align)
            .max(1);
        let (mut child_bytes, child_reloc) = child.into_parts();
        self.pad_to(child_align);
        let entry_offset = self.bytes.len();
        let ptr = u32::try_from(entry_offset).map_err(|_| BufferError::ValueTooLarge {
            name: name.to_string(),
            len: entry_offset,
        })?;
        relocate_pointers(&mut child_bytes, &child_reloc, 0, ptr).map_err(|reason| {
            BufferError::MalformedPayload {
                name: name.to_string(),
                reason,
            }
        })?;
        self.bytes.extend_from_slice(&child_bytes);
        Ok(entry_offset)
    }

    fn append_variant_record(
        &mut self,
        name: &str,
        ty: &TypeRepr,
        value: &relon_eval_api::value::Value,
    ) -> Result<usize, BufferError> {
        let (tag, payload) = variant_payload_for_value(name, ty, value)?;
        self.pad_to(variant_record_align_runtime(ty));
        let record_offset = self.bytes.len();
        self.bytes.push(tag);
        if let Some((payload_ty, payload_value)) = payload {
            let (slot_size, _) = payload_slot_layout_runtime(&payload_ty);
            let slot_offset =
                variant_payload_slot_offset(record_offset, &payload_ty).ok_or_else(|| {
                    BufferError::MalformedPayload {
                        name: name.to_string(),
                        reason: "variant payload slot offset overflows usize",
                    }
                })?;
            let slot_end = slot_offset.checked_add(slot_size).ok_or_else(|| {
                BufferError::MalformedPayload {
                    name: name.to_string(),
                    reason: "variant payload slot end overflows usize",
                }
            })?;
            if self.bytes.len() < slot_end {
                self.bytes.resize(slot_end, 0);
            }
            self.write_value_slot(name, slot_offset, &payload_ty, payload_value)?;
        }
        Ok(record_offset)
    }

    /// Append a `List<element>` payload (a pointer-array `[len][off_i]…`
    /// header plus the per-element records it points at, or an inline
    /// `[len][payload]` record for inline-fixed scalar elements) at the
    /// current tail end, **without** wiring any fixed-area field slot, and
    /// return the buffer-relative offset of the header. The returned
    /// offset is the value a field / entry slot should hold to reach this
    /// list. Recurses for `List<List<…>>` so a doubly-nested pointer array
    /// is laid out bit-identically to the field-slot writers (the reader
    /// and verifier walk the same shape). Every emitted offset is
    /// child-buffer-relative, so the existing relocation walk rebases the
    /// whole graph when the buffer is later pasted / arena-rebased.
    ///
    /// This is the recursive input marshaller behind `List<List<String>>`
    /// / `List<List<Schema>>` params (F5): the layout pass admits those
    /// shapes and the relocation walker's `PtrArrayElem::InnerList`
    /// descriptor rebases the inner pointer arrays.
    fn append_list_payload(
        &mut self,
        element: &TypeRepr,
        items: &[relon_eval_api::value::Value],
    ) -> Result<usize, BufferError> {
        use relon_eval_api::value::Value;
        let count = items.len();
        if count > u32::MAX as usize {
            return Err(BufferError::ValueTooLarge {
                name: "<nested list>".to_string(),
                len: count,
            });
        }
        match element {
            // Inline-fixed scalar element: one self-contained
            // `[len][pad][payload]` record, byte-identical to
            // `write_list_int` / `write_list_float` / `write_list_bool`.
            TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool => {
                let (inner_align, mut payload) = (
                    match element {
                        TypeRepr::Int | TypeRepr::Float => 8usize,
                        _ => 4usize,
                    },
                    Vec::<u8>::new(),
                );
                for (i, it) in items.iter().enumerate() {
                    match (element, it) {
                        (TypeRepr::Int, Value::Int(v)) => {
                            payload.extend_from_slice(&v.to_le_bytes())
                        }
                        (TypeRepr::Float, Value::Float(v)) => {
                            payload.extend_from_slice(&v.into_inner().to_le_bytes())
                        }
                        (TypeRepr::Float, Value::Int(v)) => {
                            payload.extend_from_slice(&(*v as f64).to_le_bytes())
                        }
                        (TypeRepr::Bool, Value::Bool(v)) => payload.push(u8::from(*v)),
                        (_, other) => {
                            return Err(BufferError::TypeMismatch {
                                name: format!("<nested list>[{i}]"),
                                declared: "List<scalar>",
                                requested: other.type_name(),
                            })
                        }
                    }
                }
                Ok(self.append_tail_record_with_inner_alignment(4, inner_align, count, &payload))
            }
            // `List<String>`: pointer-array header whose entries point at
            // `[len][utf8]` String records — identical to
            // `write_list_string`.
            TypeRepr::String => {
                self.pad_to(4);
                let header_offset = self.bytes.len();
                self.bytes.extend_from_slice(&(count as u32).to_le_bytes());
                let entries_start = self.bytes.len();
                self.bytes.resize(entries_start + count * 4, 0);
                let mut offsets: Vec<u32> = Vec::with_capacity(count);
                for (i, it) in items.iter().enumerate() {
                    let Value::String(s) = it else {
                        return Err(BufferError::TypeMismatch {
                            name: format!("<nested list>[{i}]"),
                            declared: "List<String>",
                            requested: it.type_name(),
                        });
                    };
                    let bytes = s.as_str().as_bytes();
                    if bytes.len() > u32::MAX as usize {
                        return Err(BufferError::ValueTooLarge {
                            name: format!("<nested list>[{i}]"),
                            len: bytes.len(),
                        });
                    }
                    let off = self.append_tail_record(4, bytes.len(), bytes);
                    offsets.push(u32::try_from(off).map_err(|_| BufferError::ValueTooLarge {
                        name: "<nested list>".to_string(),
                        len: off,
                    })?);
                }
                for (i, off) in offsets.iter().enumerate() {
                    let dst = entries_start + i * 4;
                    self.bytes[dst..dst + 4].copy_from_slice(&off.to_le_bytes());
                }
                Ok(header_offset)
            }
            // `List<List<…>>`: pointer-array header whose entries point at
            // inner list headers — recurse.
            TypeRepr::List { element: inner } => {
                self.pad_to(4);
                let header_offset = self.bytes.len();
                self.bytes.extend_from_slice(&(count as u32).to_le_bytes());
                let entries_start = self.bytes.len();
                self.bytes.resize(entries_start + count * 4, 0);
                let mut offsets: Vec<u32> = Vec::with_capacity(count);
                for (i, it) in items.iter().enumerate() {
                    let Value::List(inner_items) = it else {
                        return Err(BufferError::TypeMismatch {
                            name: format!("<nested list>[{i}]"),
                            declared: "List<List<…>>",
                            requested: it.type_name(),
                        });
                    };
                    let off = self.append_list_payload(inner, inner_items)?;
                    offsets.push(u32::try_from(off).map_err(|_| BufferError::ValueTooLarge {
                        name: "<nested list>".to_string(),
                        len: off,
                    })?);
                }
                for (i, off) in offsets.iter().enumerate() {
                    let dst = entries_start + i * 4;
                    self.bytes[dst..dst + 4].copy_from_slice(&off.to_le_bytes());
                }
                Ok(header_offset)
            }
            // `List<Schema>`: pointer-array header whose entries point at
            // schema sub-records. Each sub-record is built in a detached
            // child builder, relocated to its entry offset (so its own
            // pointer slots become buffer-relative), and pasted — exactly
            // the bytes `ListRecordWriter` produces.
            TypeRepr::Schema { schema } => {
                let sub_layout = SchemaLayout::offsets_for(schema).map_err(|_| {
                    BufferError::MalformedPayload {
                        name: "<nested list>".to_string(),
                        reason: "inner List<Schema> element schema is not layoutable",
                    }
                })?;
                // Paste each inner schema sub-record at its deep paste
                // alignment (not just `root_align`) so an 8-aligned
                // `List<Int|Float>` field in the sub-record's tail lands
                // where the reader's absolute alignment recovery expects it.
                let elem_align = schema_paste_align(&sub_layout, &schema.fields);
                self.pad_to(4);
                let header_offset = self.bytes.len();
                self.bytes.extend_from_slice(&(count as u32).to_le_bytes());
                let entries_start = self.bytes.len();
                self.bytes.resize(entries_start + count * 4, 0);
                let mut offsets: Vec<u32> = Vec::with_capacity(count);
                for (i, it) in items.iter().enumerate() {
                    let mut child = BufferBuilder::new(&sub_layout, &schema.fields);
                    match it {
                        Value::Dict(dict) if !schema.is_tuple => {
                            write_schema_record_into_builder(&mut child, schema, dict)?;
                        }
                        Value::Tuple(tuple_items) if schema.is_tuple => {
                            write_tuple_record_into_builder(&mut child, schema, tuple_items)?;
                        }
                        other => {
                            return Err(BufferError::TypeMismatch {
                                name: format!("<nested list>[{i}]"),
                                declared: if schema.is_tuple {
                                    "List<Tuple>"
                                } else {
                                    "List<Schema>"
                                },
                                requested: other.type_name(),
                            });
                        }
                    }
                    let (mut child_bytes, child_reloc) = child.into_parts();
                    self.pad_to(elem_align);
                    let entry_offset = self.bytes.len();
                    let ptr =
                        u32::try_from(entry_offset).map_err(|_| BufferError::ValueTooLarge {
                            name: "<nested list>".to_string(),
                            len: entry_offset,
                        })?;
                    relocate_pointers(&mut child_bytes, &child_reloc, 0, ptr).map_err(
                        |reason| BufferError::MalformedPayload {
                            name: format!("<nested list>[{i}]"),
                            reason,
                        },
                    )?;
                    self.bytes.extend_from_slice(&child_bytes);
                    offsets.push(ptr);
                }
                for (i, off) in offsets.iter().enumerate() {
                    let dst = entries_start + i * 4;
                    self.bytes[dst..dst + 4].copy_from_slice(&off.to_le_bytes());
                }
                Ok(header_offset)
            }
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
                self.pad_to(4);
                let header_offset = self.bytes.len();
                self.bytes.extend_from_slice(&(count as u32).to_le_bytes());
                let entries_start = self.bytes.len();
                self.bytes.resize(entries_start + count * 4, 0);
                let mut offsets: Vec<u32> = Vec::with_capacity(count);
                for (i, it) in items.iter().enumerate() {
                    let entry_name = format!("<nested list>[{i}]");
                    let off = self.append_variant_record(&entry_name, element, it)?;
                    offsets.push(u32::try_from(off).map_err(|_| BufferError::ValueTooLarge {
                        name: "<nested list>".to_string(),
                        len: off,
                    })?);
                }
                for (i, off) in offsets.iter().enumerate() {
                    let dst = entries_start + i * 4;
                    self.bytes[dst..dst + 4].copy_from_slice(&off.to_le_bytes());
                }
                Ok(header_offset)
            }
            other => Err(BufferError::TypeMismatch {
                name: "<nested list>".to_string(),
                declared: type_label(other),
                requested: "List<scalar/String/Schema/List/Option/Result>",
            }),
        }
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
        if let Some(variant_ty) = slot.variant.as_ref() {
            relocate_variant_record(bytes, original as usize, variant_ty, paste_base)?;
            continue;
        }
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
                    slot.list_elem.as_ref(),
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

/// Rebase every entry of a `List<String>` / `List<Schema>` /
/// `List<List<…>>` pointer array. `record_start` is the byte offset of
/// the list's tail record (the `[len: u32][off_0: u32]...` header). Walks
/// the `len` entries, adds `paste_base` to each, and recurses per the
/// element descriptor `elem`:
///
/// * `Schema` — into the per-element sub-record's [`RelocLayout`] so its
///   own pointer slots are rebased too.
/// * `InnerList` — the entry points at an **inner list header** that is
///   itself a pointer array; recurse into it one level deeper (this is
///   the `List<List<String|Schema>>` relocation the v1 walker lacked).
/// * `String` / `None` — the entry's target carries no internal pointer
///   (an inline-fixed inner scalar list, or a String record), so the
///   entry-pointer rebase above is all that is needed.
fn relocate_list_pointer_array(
    bytes: &mut [u8],
    record_start: usize,
    elem: Option<&PtrArrayElem>,
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
        // Recurse per element descriptor so each entry's own internal
        // pointers are rebased. `original` is the entry's pre-relocation
        // (child-buffer-relative) offset — the coordinate the inner
        // pointers were written against — so the inner walk continues to
        // add `paste_base` exactly once.
        match elem {
            Some(PtrArrayElem::Schema(element_reloc)) => {
                relocate_pointers(bytes, element_reloc, original as usize, paste_base)?;
            }
            Some(PtrArrayElem::InnerList { inner }) => {
                relocate_list_pointer_array(
                    bytes,
                    original as usize,
                    Some(inner.as_ref()),
                    paste_base,
                )?;
            }
            Some(PtrArrayElem::Variant(ty)) => {
                relocate_variant_record(bytes, original as usize, ty, paste_base)?;
            }
            Some(PtrArrayElem::String) | None => {}
        }
        cursor += 4;
    }
    Ok(())
}

fn relocate_variant_record(
    bytes: &mut [u8],
    record_start: usize,
    ty: &TypeRepr,
    paste_base: u32,
) -> Result<(), &'static str> {
    if record_start
        .checked_add(1)
        .map(|end| end > bytes.len())
        .unwrap_or(true)
    {
        return Err("variant tag exceeds buffer end");
    }
    let tag = bytes[record_start];
    let selected = variant_selected_payload(ty, tag)?;
    let Some(payload) = selected.payload else {
        return Ok(());
    };
    let payload_ty = payload.ty;
    if !matches!(
        payload_ty,
        TypeRepr::String
            | TypeRepr::List { .. }
            | TypeRepr::Schema { .. }
            | TypeRepr::Option { .. }
            | TypeRepr::Result { .. }
            | TypeRepr::Enum { .. }
    ) {
        return Ok(());
    }
    let slot_abs = variant_payload_slot_offset(record_start, &payload_ty)
        .ok_or("variant payload slot offset overflows usize")?;
    if slot_abs
        .checked_add(4)
        .map(|end| end > bytes.len())
        .unwrap_or(true)
    {
        return Err("variant payload pointer slot exceeds buffer end");
    }
    let mut ptr_buf = [0u8; 4];
    ptr_buf.copy_from_slice(&bytes[slot_abs..slot_abs + 4]);
    let original = u32::from_le_bytes(ptr_buf);
    let relocated = original
        .checked_add(paste_base)
        .ok_or("relocated variant payload pointer overflows u32")?;
    bytes[slot_abs..slot_abs + 4].copy_from_slice(&relocated.to_le_bytes());
    match &payload_ty {
        TypeRepr::String => Ok(()),
        TypeRepr::Schema { schema } => {
            let layout = SchemaLayout::offsets_for(schema)
                .map_err(|_| "variant schema payload is not layoutable")?;
            let reloc = RelocLayout::build(&layout, &schema.fields);
            relocate_pointers(bytes, &reloc, original as usize, paste_base)
        }
        TypeRepr::List { element } => {
            if list_payload_is_pointer_array(element) {
                let elem = ptr_array_elem_for(element);
                relocate_list_pointer_array(bytes, original as usize, elem.as_ref(), paste_base)?;
            }
            Ok(())
        }
        TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
            relocate_variant_record(bytes, original as usize, &payload_ty, paste_base)
        }
        _ => Ok(()),
    }
}

/// Marshal a nested `List<List<scalar>>` arg / schema field into
/// `field_name`'s tail area, given the inner element [`TypeRepr`] and
/// the outer `Value::List` items (each element itself a `Value::List`
/// of inline-fixed scalars). Shared by both compiled backends so they
/// emit byte-identical input buffers. Inner pointer-array element lists
/// (`List<List<String>>` / `List<List<Schema>>`) are rejected by the
/// layout pass before reaching here; this routine only handles the
/// inline-fixed innermost elements (`Int` / `Float` / `Bool`).
pub fn write_nested_scalar_list(
    builder: &mut BufferBuilder<'_>,
    field_name: &str,
    inner: &TypeRepr,
    items: &[relon_eval_api::value::Value],
) -> Result<(), BufferError> {
    use relon_eval_api::value::Value;
    builder.write_list_list_with(field_name, items.len(), |i, payload| {
        let Value::List(inner_items) = &items[i] else {
            return Err(BufferError::TypeMismatch {
                name: format!("{field_name}[{i}]"),
                declared: "List<List<…>>",
                requested: "List",
            });
        };
        match inner {
            TypeRepr::Int => {
                for it in inner_items.iter() {
                    let Value::Int(v) = it else {
                        return Err(BufferError::TypeMismatch {
                            name: format!("{field_name}[{i}]"),
                            declared: "List<Int>",
                            requested: "List<Int>",
                        });
                    };
                    payload.extend_from_slice(&v.to_le_bytes());
                }
                Ok((inner_items.len(), 8))
            }
            TypeRepr::Float => {
                for it in inner_items.iter() {
                    let f = match it {
                        Value::Float(v) => v.into_inner(),
                        // Int → Float promotion, matching the scalar arm.
                        Value::Int(v) => *v as f64,
                        _ => {
                            return Err(BufferError::TypeMismatch {
                                name: format!("{field_name}[{i}]"),
                                declared: "List<Float>",
                                requested: "List<Float>",
                            })
                        }
                    };
                    payload.extend_from_slice(&f.to_le_bytes());
                }
                Ok((inner_items.len(), 8))
            }
            TypeRepr::Bool => {
                for it in inner_items.iter() {
                    let Value::Bool(v) = it else {
                        return Err(BufferError::TypeMismatch {
                            name: format!("{field_name}[{i}]"),
                            declared: "List<Bool>",
                            requested: "List<Bool>",
                        });
                    };
                    payload.push(u8::from(*v));
                }
                Ok((inner_items.len(), 4))
            }
            _ => Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: "List<List<scalar>>",
                requested: "List<List<pointer-array element>>",
            }),
        }
    })
}

/// Marshal a `List<List<String>>` / `List<List<Schema>>` (F5: a doubly-
/// nested pointer-array list, where each outer element is itself a
/// pointer-array list) into `field_name`'s tail area. `inner_element` is
/// the **innermost** element type (`String` / `Schema` / a deeper
/// `List<…>`), matching the `marshal_list_list_in` dispatch contract;
/// `items` are the outer `Value::List` rows (each itself a
/// `List<inner_element>`). The whole nested structure is written at
/// child-buffer-relative offsets and the field's pointer slot patched to
/// the outer header — the relocation walker's `PtrArrayElem::InnerList`
/// descriptor rebases the inner pointer arrays on the later arena rebase.
///
/// Shared by both compiled backends so they emit byte-identical input
/// buffers. The layout pass admits these shapes (`inner_list_record_
/// alignment`); the inline-fixed `List<List<scalar>>` case keeps using
/// [`write_nested_scalar_list`].
pub fn write_nested_pointer_array_list(
    builder: &mut BufferBuilder<'_>,
    field_name: &str,
    inner_element: &TypeRepr,
    items: &[relon_eval_api::value::Value],
) -> Result<(), BufferError> {
    // The field slot must be a pointer-indirect `List<List<…>>` slot.
    let slot_offset = {
        let entry = builder
            .field_index
            .iter()
            .find(|e| e.name == field_name)
            .ok_or_else(|| BufferError::UnknownField {
                name: field_name.to_string(),
            })?;
        let is_nested = matches!(&entry.ty, TypeRepr::List { element: outer }
            if matches!(outer.as_ref(), TypeRepr::List { .. }));
        if !is_nested || !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: "List<List<String|Schema>>",
            });
        }
        entry.offset
    };
    // `inner_element` is the innermost element (`String` / `Schema`).
    // Build the outer pointer array whose entries each point at an inner
    // `List<inner_element>` written by recursion.
    let header = append_outer_pointer_array(builder, inner_element, items)?;
    let ptr = u32::try_from(header).map_err(|_| BufferError::ValueTooLarge {
        name: field_name.to_string(),
        len: header,
    })?;
    builder.bytes[slot_offset..slot_offset + 4].copy_from_slice(&ptr.to_le_bytes());
    Ok(())
}

/// Write the outer pointer-array header for a `List<List<inner_element>>`
/// whose **innermost** element type is `inner_element` (`String` /
/// `Schema` / a deeper `List<…>`), returning the header's buffer-relative
/// offset. Each outer entry is a `List<inner_element>` value
/// (`Value::List`), serialised via [`BufferBuilder::append_list_payload`].
///
/// Mirrors the marshaller dispatch contract (`marshal_list_list_in`):
/// `inner_element` is the inner-list element, **not** the outer list
/// element — so for `List<List<String>>` the callers pass `String`.
fn append_outer_pointer_array(
    builder: &mut BufferBuilder<'_>,
    inner_element: &TypeRepr,
    items: &[relon_eval_api::value::Value],
) -> Result<usize, BufferError> {
    use relon_eval_api::value::Value;
    let count = items.len();
    if count > u32::MAX as usize {
        return Err(BufferError::ValueTooLarge {
            name: "<nested list>".to_string(),
            len: count,
        });
    }
    builder.pad_to(4);
    let header_offset = builder.bytes.len();
    builder
        .bytes
        .extend_from_slice(&(count as u32).to_le_bytes());
    let entries_start = builder.bytes.len();
    builder.bytes.resize(entries_start + count * 4, 0);
    let mut offsets: Vec<u32> = Vec::with_capacity(count);
    for (i, it) in items.iter().enumerate() {
        let Value::List(inner_items) = it else {
            return Err(BufferError::TypeMismatch {
                name: format!("<nested list>[{i}]"),
                declared: "List<List<…>>",
                requested: it.type_name(),
            });
        };
        // Each outer entry is a `List<inner_element>`; serialise it.
        let off = builder.append_list_payload(inner_element, inner_items)?;
        offsets.push(u32::try_from(off).map_err(|_| BufferError::ValueTooLarge {
            name: "<nested list>".to_string(),
            len: off,
        })?);
    }
    for (i, off) in offsets.iter().enumerate() {
        let dst = entries_start + i * 4;
        builder.bytes[dst..dst + 4].copy_from_slice(&off.to_le_bytes());
    }
    Ok(header_offset)
}

/// Write every field of a `#schema` record (`schema`) from a branded
/// `Value::Dict` into `child`. Generic over field type — scalars,
/// `String`, `List<scalar/String/Schema/List>`, and nested `Schema`
/// sub-records — so the recursive nested-list marshaller has one
/// self-contained schema writer that produces bytes identical to the
/// per-backend `write_value_into_builder`. A missing / mistyped field is
/// a loud error.
fn write_schema_record_into_builder(
    child: &mut BufferBuilder<'_>,
    schema: &Schema,
    dict: &relon_eval_api::value::ValueDict,
) -> Result<(), BufferError> {
    for field in &schema.fields {
        let value =
            dict.map
                .get(field.name.as_str())
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: field.name.clone(),
                    reason: "schema record is missing a declared field",
                })?;
        write_schema_field_into_builder(child, field, value)?;
    }
    Ok(())
}

/// Write every field of a tuple schema from a positional `Value::Tuple`.
/// The schema is still a record at the binary layer; only the source
/// container shape is positional.
fn write_tuple_record_into_builder(
    child: &mut BufferBuilder<'_>,
    schema: &Schema,
    items: &[relon_eval_api::value::Value],
) -> Result<(), BufferError> {
    if items.len() != schema.fields.len() {
        return Err(BufferError::MalformedPayload {
            name: schema.name.clone(),
            reason: "tuple arity does not match schema",
        });
    }
    for (field, value) in schema.fields.iter().zip(items.iter()) {
        write_schema_field_into_builder(child, field, value)?;
    }
    Ok(())
}

/// Write one schema field (`field`) carrying `value` into `child`.
fn write_schema_field_into_builder(
    child: &mut BufferBuilder<'_>,
    field: &Field,
    value: &relon_eval_api::value::Value,
) -> Result<(), BufferError> {
    child.write_value(field.name.as_str(), &field.ty, value)
}

/// `(discriminant, optional (payload type, payload value))` produced when
/// matching a value against a variant slot.
type VariantPayloadSlot<'a> = (u8, Option<(TypeRepr, &'a relon_eval_api::value::Value)>);

fn variant_payload_for_value<'a>(
    name: &str,
    ty: &'a TypeRepr,
    value: &'a relon_eval_api::value::Value,
) -> Result<VariantPayloadSlot<'a>, BufferError> {
    let relon_eval_api::value::Value::Dict(dict) = value else {
        return Err(BufferError::TypeMismatch {
            name: name.to_string(),
            declared: type_label(ty),
            requested: value.type_name(),
        });
    };
    match ty {
        TypeRepr::Option { inner } => match (dict.variant_of.as_deref(), dict.brand.as_deref()) {
            (Some("Option"), Some("None")) => Ok((0, None)),
            (Some("Option"), Some("Some")) => {
                let payload =
                    dict.map
                        .get("value")
                        .ok_or_else(|| BufferError::MalformedPayload {
                            name: name.to_string(),
                            reason: "Option.Some is missing `value` payload",
                        })?;
                Ok((1, Some((inner.as_ref().clone(), payload))))
            }
            _ => Err(BufferError::TypeMismatch {
                name: name.to_string(),
                declared: "Option",
                requested: value.type_name(),
            }),
        },
        TypeRepr::Result { ok, err } => match (dict.variant_of.as_deref(), dict.brand.as_deref()) {
            (Some("Result"), Some("Ok")) => {
                let payload =
                    dict.map
                        .get("value")
                        .ok_or_else(|| BufferError::MalformedPayload {
                            name: name.to_string(),
                            reason: "Result.Ok is missing `value` payload",
                        })?;
                Ok((0, Some((ok.as_ref().clone(), payload))))
            }
            (Some("Result"), Some("Err")) => {
                let payload =
                    dict.map
                        .get("error")
                        .ok_or_else(|| BufferError::MalformedPayload {
                            name: name.to_string(),
                            reason: "Result.Err is missing `error` payload",
                        })?;
                Ok((1, Some((err.as_ref().clone(), payload))))
            }
            _ => Err(BufferError::TypeMismatch {
                name: name.to_string(),
                declared: "Result",
                requested: value.type_name(),
            }),
        },
        TypeRepr::Enum {
            name: enum_name,
            variants,
        } => {
            let (Some(value_enum), Some(value_variant)) =
                (dict.variant_of.as_deref(), dict.brand.as_deref())
            else {
                return Err(BufferError::TypeMismatch {
                    name: name.to_string(),
                    declared: "Enum",
                    requested: value.type_name(),
                });
            };
            if value_enum != enum_name {
                return Err(BufferError::TypeMismatch {
                    name: name.to_string(),
                    declared: "Enum",
                    requested: value.type_name(),
                });
            }
            let variant = variants
                .iter()
                .find(|variant| variant.name == value_variant)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: name.to_string(),
                    reason: "enum value carries an unknown variant",
                })?;
            if variant.fields.is_empty() {
                if !dict.map.is_empty() {
                    return Err(BufferError::MalformedPayload {
                        name: name.to_string(),
                        reason: "unit enum variant carries payload fields",
                    });
                }
                return Ok((variant.tag, None));
            }
            let payload_ty = TypeRepr::Schema {
                schema: Box::new(variant.payload_schema(enum_name).ok_or_else(|| {
                    BufferError::MalformedPayload {
                        name: name.to_string(),
                        reason: "enum payload schema is missing",
                    }
                })?),
            };
            Ok((variant.tag, Some((payload_ty, value))))
        }
        _ => Err(BufferError::TypeMismatch {
            name: name.to_string(),
            declared: type_label(ty),
            requested: value.type_name(),
        }),
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

    /// Build a reader whose root record's fixed area is anchored at the
    /// arena-absolute offset `record_base` inside the whole-arena slice
    /// `bytes`, rather than at offset `0`.
    ///
    /// This is the F1 object-return decode entry point: under the
    /// arena-absolute slot convention the object head lives at `out_ptr`
    /// and every pointer slot it carries holds an arena-absolute offset,
    /// so the reader walks the **whole arena** (`bytes`) and each field
    /// slot is read at `record_base + fo.offset`. Mirrors the per-entry
    /// field-index rebase [`Self::read_list_record_at`] /
    /// [`Self::sub_record`] already perform, so a subsequent
    /// pointer-indirect read resolves its arena-absolute slot value
    /// directly against `bytes`.
    pub fn new_at_base(
        layout: &'a OffsetTable,
        fields: &[Field],
        bytes: &'a [u8],
        record_base: usize,
    ) -> Result<Self, BufferError> {
        let record_end =
            record_base
                .checked_add(layout.root_size)
                .ok_or(BufferError::BufferTooSmall {
                    have: bytes.len(),
                    need: usize::MAX,
                })?;
        if record_end > bytes.len() {
            return Err(BufferError::BufferTooSmall {
                have: bytes.len(),
                need: record_end,
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
                        offset: record_base + fo.offset,
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

    /// Confirm `field_name` is declared as an internal unit slot and that the slot
    /// is reachable. The byte value is unused (Unit slots are
    /// tag-only), so this only validates the type label.
    pub fn read_unit(&self, field_name: &str) -> Result<(), BufferError> {
        let (_, _, _) = self.locate(field_name, &TypeRepr::Unit, "Unit")?;
        Ok(())
    }

    /// Read any supported value shape from `field_name` using its canonical
    /// type. Mirrors [`BufferBuilder::write_value`].
    pub fn read_value(
        &self,
        field_name: &str,
        ty: &TypeRepr,
    ) -> Result<relon_eval_api::value::Value, BufferError> {
        let entry = self.find_entry(field_name)?;
        if !type_matches(&entry.ty, ty) {
            return Err(BufferError::TypeMismatch {
                name: field_name.to_string(),
                declared: type_label(&entry.ty),
                requested: type_label(ty),
            });
        }
        self.read_field_at(entry.offset, ty)
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
        self.check_pointer_entries_in_bounds(field_name, entries_start, count)?;
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

    /// Read a `List<String>` whose pointer-array header sits **directly**
    /// at `header_off` (rather than behind a named fixed-area slot). This
    /// is the in-place region-walk return entry point (S3): the machine
    /// code reports the arena-relative offset of the outer
    /// `[len][off_0]…[off_{N-1}]` header, the host rebases it to
    /// `header_off`, and — after the verifier certifies the whole graph
    /// stays in-region — this walks the same bytes [`read_list_string`]
    /// would, so a top-level (field-slot) and an in-place decode of the
    /// same buffer are byte-identical, including each string's content.
    ///
    /// Every offset / length is bounds-checked against the buffer end
    /// before any read (the verifier already proved the tighter
    /// single-region bound); a non-UTF-8 payload is a loud error, matching
    /// [`read_list_string`] and the `Value::String` invariant (Relon
    /// strings are always valid UTF-8).
    pub fn read_list_string_at(&self, header_off: usize) -> Result<Vec<&'a str>, BufferError> {
        // Header: `[len: u32][off_0]…`. `entries_start = header_off + 4`.
        if header_off
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<in-place root>".to_string(),
                reason: "in-place list-string header length prefix exceeds buffer end",
            });
        }
        let mut len_buf = [0u8; 4];
        len_buf.copy_from_slice(&self.bytes[header_off..header_off + 4]);
        let count = u32::from_le_bytes(len_buf) as usize;
        let entries_start = header_off + 4;
        self.check_pointer_entries_in_bounds("<in-place root>", entries_start, count)?;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let cursor =
                entries_start
                    .checked_add(i * 4)
                    .ok_or_else(|| BufferError::MalformedPayload {
                        name: "<in-place root>".to_string(),
                        reason: "in-place list-string entry cursor overflows usize",
                    })?;
            if cursor + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-string entry pointer exceeds buffer end",
                });
            }
            let mut entry_buf = [0u8; 4];
            entry_buf.copy_from_slice(&self.bytes[cursor..cursor + 4]);
            let record_start = u32::from_le_bytes(entry_buf) as usize;
            if record_start + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-string len prefix exceeds buffer end",
                });
            }
            let mut sl_buf = [0u8; 4];
            sl_buf.copy_from_slice(&self.bytes[record_start..record_start + 4]);
            let str_len = u32::from_le_bytes(sl_buf) as usize;
            let payload_start = record_start + 4;
            let payload_end = payload_start.checked_add(str_len).ok_or_else(|| {
                BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-string payload end overflows usize",
                }
            })?;
            if payload_end > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-string payload exceeds buffer end",
                });
            }
            let s = std::str::from_utf8(&self.bytes[payload_start..payload_end]).map_err(|_| {
                BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-string payload is not valid utf-8",
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
        self.check_pointer_entries_in_bounds(field_name, entries_start, count)?;
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

    /// Read a `List<Schema>` whose **outer pointer-array header** sits
    /// directly at `header_off` (a region-relative offset into
    /// `self.bytes`), rather than being reached through a record's
    /// fixed-area slot.
    ///
    /// This is the reader half of the in-place region-walk return ABI
    /// (S4). The machine code returns the arena-absolute offset of the
    /// root list header `[len][off_0]…[off_{N-1}]`; the host rebases it to
    /// a region-relative offset, runs [`crate::verifier::verify_value_at`]
    /// over the whole reachable graph (outer entries → each sub-record's
    /// fixed area → every String / List field pointer the sub-record
    /// carries), and only then calls this to decode in place. Each entry
    /// pointer names a sub-record anchored at `elem_layout`; a
    /// [`BufferReader`] is produced per entry sharing the same buffer
    /// slice, so subsequent field reads (`read_string`, `read_list_int`,
    /// …) resolve back into the same tail area — bit-identical to the
    /// field-slot [`Self::read_list_record`] path the tree-walk oracle's
    /// writer feeds.
    pub fn read_list_record_at<'b>(
        &self,
        header_off: usize,
        elem_layout: &'b OffsetTable,
        elem_schema: &'b Schema,
    ) -> Result<Vec<BufferReader<'a>>, BufferError>
    where
        'b: 'a,
    {
        // Header sits directly at `header_off`: `[len][off_0]…`.
        if header_off
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<in-place root>".to_string(),
                reason: "in-place list-record header length prefix exceeds buffer end",
            });
        }
        let mut len_buf = [0u8; 4];
        len_buf.copy_from_slice(&self.bytes[header_off..header_off + 4]);
        let count = u32::from_le_bytes(len_buf) as usize;
        let entries_start = header_off + 4;
        self.check_pointer_entries_in_bounds("<in-place root>", entries_start, count)?;
        let mut out: Vec<BufferReader<'a>> = Vec::with_capacity(count);
        for i in 0..count {
            let cursor =
                entries_start
                    .checked_add(i * 4)
                    .ok_or_else(|| BufferError::MalformedPayload {
                        name: "<in-place root>".to_string(),
                        reason: "in-place list-record entry cursor overflows usize",
                    })?;
            if cursor + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-record entry pointer exceeds buffer end",
                });
            }
            let mut entry_buf = [0u8; 4];
            entry_buf.copy_from_slice(&self.bytes[cursor..cursor + 4]);
            let sub_base = u32::from_le_bytes(entry_buf) as usize;
            let sub_end = sub_base.checked_add(elem_layout.root_size).ok_or_else(|| {
                BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-record sub-record end overflows usize",
                }
            })?;
            if sub_end > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list-record sub-record exceeds buffer end",
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

    /// Recursively decode a `List<element>` whose **header sits directly
    /// at `header_off`** (a region-relative offset into `self.bytes`) into
    /// a `Vec<Value>`, dispatching on the declared `element` type. This is
    /// the unified in-place list reader behind the F5 doubly-nested
    /// pointer-array shapes (`List<List<String>>` / `List<List<Schema>>`):
    /// the outer call reads the outer pointer array, and each entry —
    /// being itself a `List<inner>` — recurses one level deeper, exactly
    /// the depth the verifier already certified. Every offset / length is
    /// bounds-checked against `self.bytes`; a non-UTF-8 String payload is a
    /// loud error. The produced values are bit-identical to the field-slot
    /// readers the tree-walk oracle's writer feeds.
    pub fn read_list_value_at(
        &self,
        header_off: usize,
        element: &TypeRepr,
    ) -> Result<Vec<relon_eval_api::value::Value>, BufferError> {
        use relon_eval_api::value::Value;
        match element {
            // Inline-fixed scalar inner list / pointer-array String /
            // nested-list element: dispatch through the existing readers.
            TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool => {
                // A `List<scalar>` whose header is at `header_off`:
                // `[len][pad][payload]`. Decode directly.
                self.read_inline_scalar_list_at(header_off, element)
            }
            TypeRepr::String => Ok(self
                .read_list_string_at(header_off)?
                .into_iter()
                .map(|s| Value::String(s.into()))
                .collect()),
            TypeRepr::List { element: inner } => {
                // Outer pointer array; recurse per entry into the inner
                // list it points at.
                let (count, entries_start) = self.read_pointer_array_header(header_off)?;
                let mut out = Vec::with_capacity(count);
                for i in 0..count {
                    let entry = self.read_entry_pointer(entries_start, i)?;
                    let inner_vals = self.read_list_value_at(entry, inner)?;
                    out.push(Value::List(std::sync::Arc::new(inner_vals)));
                }
                Ok(out)
            }
            TypeRepr::Schema { schema } => {
                // Outer pointer array; each entry points at a sub-record's
                // fixed area. Decode each at its absolute base recursively.
                let elem_layout = SchemaLayout::offsets_for(schema).map_err(|_| {
                    BufferError::MalformedPayload {
                        name: "<in-place root>".to_string(),
                        reason: "inner List<Schema> element schema is not layoutable",
                    }
                })?;
                let (count, entries_start) = self.read_pointer_array_header(header_off)?;
                let mut out = Vec::with_capacity(count);
                for i in 0..count {
                    let sub_base = self.read_entry_pointer(entries_start, i)?;
                    let sub_end = sub_base.checked_add(elem_layout.root_size).ok_or_else(|| {
                        BufferError::MalformedPayload {
                            name: "<in-place root>".to_string(),
                            reason: "in-place sub-record end overflows usize",
                        }
                    })?;
                    if sub_end > self.bytes.len() {
                        return Err(BufferError::MalformedPayload {
                            name: "<in-place root>".to_string(),
                            reason: "in-place sub-record exceeds buffer end",
                        });
                    }
                    out.push(self.read_record_at(sub_base, &elem_layout, schema)?);
                }
                Ok(out)
            }
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
                let (count, entries_start) = self.read_pointer_array_header(header_off)?;
                let mut out = Vec::with_capacity(count);
                for i in 0..count {
                    let variant_base = self.read_entry_pointer(entries_start, i)?;
                    out.push(self.read_variant_record_at(variant_base, element)?);
                }
                Ok(out)
            }
            other => Err(BufferError::TypeMismatch {
                name: "<in-place root>".to_string(),
                declared: type_label(other),
                requested: "List<scalar/String/Schema/List/Option/Result>",
            }),
        }
    }

    /// Field-slot entry point for the recursive list reader: resolve the
    /// pointer-indirect `field_name` slot to its list header offset, then
    /// decode through [`Self::read_list_value_at`]. `element` is the
    /// outer list's element type. Used by the object / sub-record decode
    /// path so a `List<List<String|Schema>>` field decodes to the same
    /// `Vec<Value>` the in-place return path produces.
    pub fn read_list_value(
        &self,
        field_name: &str,
        element: &TypeRepr,
    ) -> Result<Vec<relon_eval_api::value::Value>, BufferError> {
        let entry = self.find_entry(field_name)?;
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect list slot",
            });
        }
        let header_off = self.read_slot_pointer(entry.offset)?;
        self.read_list_value_at(header_off, element)
    }

    /// Validate that a pointer-array's `count` 4-byte entries all lie
    /// within the buffer **before** any capacity-sized allocation.
    ///
    /// A pointer-array header stores `count` as an untrusted `u32` (up to
    /// ~4.29e9). Calling `Vec::with_capacity(count)` on it directly lets a
    /// malformed buffer request hundreds of gigabytes and abort the
    /// process (OOM / allocation-failure DoS). Bounding `count` by the
    /// buffer size here — the entry region `[entries_start, +count*4)` must
    /// fit — makes the speculative allocation safe, mirroring the
    /// `payload_end` interval guard the scalar list readers
    /// (`read_list_int` / `read_list_float` / `read_inline_scalar_list_at`)
    /// already apply. O(1); the per-entry bounds checks stay as a
    /// belt-and-braces second line.
    fn check_pointer_entries_in_bounds(
        &self,
        field_name: &str,
        entries_start: usize,
        count: usize,
    ) -> Result<(), BufferError> {
        let end = count
            .checked_mul(4)
            .and_then(|bytes| entries_start.checked_add(bytes));
        match end {
            Some(end) if end <= self.bytes.len() => Ok(()),
            _ => Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "list pointer-array entries exceed buffer end",
            }),
        }
    }

    /// Read the `[len][off_i]…` pointer-array header at `header_off`,
    /// returning `(count, entries_start)` after bounds-checking the length
    /// prefix **and** the full `count`-entry region (see
    /// [`Self::check_pointer_entries_in_bounds`]).
    fn read_pointer_array_header(&self, header_off: usize) -> Result<(usize, usize), BufferError> {
        if header_off
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<in-place root>".to_string(),
                reason: "in-place list header length prefix exceeds buffer end",
            });
        }
        let mut len_buf = [0u8; 4];
        len_buf.copy_from_slice(&self.bytes[header_off..header_off + 4]);
        let count = u32::from_le_bytes(len_buf) as usize;
        let entries_start = header_off + 4;
        self.check_pointer_entries_in_bounds("<in-place root>", entries_start, count)?;
        Ok((count, entries_start))
    }

    /// Read the `i`-th entry pointer (a `u32`) of a pointer array whose
    /// entries start at `entries_start`, bounds-checked.
    fn read_entry_pointer(&self, entries_start: usize, i: usize) -> Result<usize, BufferError> {
        let cursor =
            entries_start
                .checked_add(i * 4)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "in-place list entry cursor overflows usize",
                })?;
        if cursor + 4 > self.bytes.len() {
            return Err(BufferError::MalformedPayload {
                name: "<in-place root>".to_string(),
                reason: "in-place list entry pointer exceeds buffer end",
            });
        }
        let mut entry_buf = [0u8; 4];
        entry_buf.copy_from_slice(&self.bytes[cursor..cursor + 4]);
        Ok(u32::from_le_bytes(entry_buf) as usize)
    }

    /// Decode a `List<scalar>` whose `[len][pad][payload]` record sits at
    /// `header_off`, dispatching on the scalar element type. Mirrors the
    /// field-slot `read_list_int` / `read_list_float` / `read_list_bool`
    /// element decode.
    fn read_inline_scalar_list_at(
        &self,
        header_off: usize,
        element: &TypeRepr,
    ) -> Result<Vec<relon_eval_api::value::Value>, BufferError> {
        use relon_eval_api::value::Value;
        if header_off
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<in-place root>".to_string(),
                reason: "inline scalar list header length prefix exceeds buffer end",
            });
        }
        let mut len_buf = [0u8; 4];
        len_buf.copy_from_slice(&self.bytes[header_off..header_off + 4]);
        let count = u32::from_le_bytes(len_buf) as usize;
        let (elem_size, align): (usize, usize) = match element {
            TypeRepr::Int | TypeRepr::Float => (8, 8),
            TypeRepr::Bool => (1, 1),
            other => {
                return Err(BufferError::TypeMismatch {
                    name: "<in-place root>".to_string(),
                    declared: type_label(other),
                    requested: "List<scalar>",
                })
            }
        };
        let payload_start = if align > 1 {
            (header_off + 4).next_multiple_of(align)
        } else {
            header_off + 4
        };
        let byte_len =
            count
                .checked_mul(elem_size)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "inline scalar list byte length overflows usize",
                })?;
        let end =
            payload_start
                .checked_add(byte_len)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: "inline scalar list payload end overflows usize",
                })?;
        if end > self.bytes.len() {
            return Err(BufferError::MalformedPayload {
                name: "<in-place root>".to_string(),
                reason: "inline scalar list payload exceeds buffer end",
            });
        }
        let mut out = Vec::with_capacity(count);
        for k in 0..count {
            let off = payload_start + k * elem_size;
            match element {
                TypeRepr::Int => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&self.bytes[off..off + 8]);
                    out.push(Value::Int(i64::from_le_bytes(b)));
                }
                TypeRepr::Float => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&self.bytes[off..off + 8]);
                    out.push(Value::Float(ordered_float::OrderedFloat(
                        f64::from_le_bytes(b),
                    )));
                }
                TypeRepr::Bool => out.push(Value::Bool(self.bytes[off] != 0)),
                _ => unreachable!("scalar element validated above"),
            }
        }
        Ok(out)
    }

    /// Decode a `#schema` sub-record whose fixed area sits at the
    /// arena/region offset `record_base` into a branded `Value::Dict`,
    /// reading each field at `record_base + fo.offset`. Generic over field
    /// type — scalars, `String`, `List<…>` (via the recursive
    /// [`Self::read_list_value_at`]), and nested `Schema` — so a
    /// `List<List<Schema>>` element or a sub-record carrying a
    /// `List<List<String>>` field decodes to the same `Value` the
    /// tree-walk oracle produces. Bounds are checked at each pointer
    /// dereference; the verifier has already certified the whole graph.
    fn read_record_at(
        &self,
        record_base: usize,
        layout: &OffsetTable,
        schema: &Schema,
    ) -> Result<relon_eval_api::value::Value, BufferError> {
        use relon_eval_api::smol_str::SmolStr;
        use relon_eval_api::value::Value;
        if schema.is_tuple {
            let mut items = Vec::with_capacity(schema.fields.len());
            for field in &schema.fields {
                let fo = layout
                    .fields
                    .iter()
                    .find(|fo| fo.name == field.name)
                    .ok_or_else(|| BufferError::MalformedPayload {
                        name: field.name.clone(),
                        reason: "tuple field missing from layout",
                    })?;
                let slot_abs = record_base.checked_add(fo.offset).ok_or_else(|| {
                    BufferError::MalformedPayload {
                        name: field.name.clone(),
                        reason: "tuple field slot offset overflows usize",
                    }
                })?;
                items.push(self.read_field_at(slot_abs, &field.ty)?);
            }
            return Ok(Value::Tuple(std::sync::Arc::new(items)));
        }
        let mut map = std::collections::BTreeMap::new();
        for field in &schema.fields {
            let fo = layout
                .fields
                .iter()
                .find(|fo| fo.name == field.name)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: field.name.clone(),
                    reason: "schema field missing from layout",
                })?;
            let slot_abs = record_base.checked_add(fo.offset).ok_or_else(|| {
                BufferError::MalformedPayload {
                    name: field.name.clone(),
                    reason: "field slot offset overflows usize",
                }
            })?;
            let v = self.read_field_at(slot_abs, &field.ty)?;
            map.insert(SmolStr::from(field.name.as_str()), v);
        }
        Ok(Value::branded_dict(map, Some(schema.name.clone())))
    }

    /// Decode one field of declared type `ty` whose fixed-area slot sits
    /// at the absolute offset `slot_abs`. Scalars read inline; pointer-
    /// indirect fields (`String` / `List<…>` / `Schema`) read the `u32`
    /// slot and recurse.
    fn read_field_at(
        &self,
        slot_abs: usize,
        ty: &TypeRepr,
    ) -> Result<relon_eval_api::value::Value, BufferError> {
        use relon_eval_api::value::Value;
        let read_inline = |abs: usize, len: usize| -> Result<&[u8], BufferError> {
            let end = abs
                .checked_add(len)
                .ok_or_else(|| BufferError::MalformedPayload {
                    name: "<sub-record field>".to_string(),
                    reason: "inline field span overflows usize",
                })?;
            if end > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: "<sub-record field>".to_string(),
                    reason: "inline field span exceeds buffer end",
                });
            }
            Ok(&self.bytes[abs..end])
        };
        match ty {
            TypeRepr::Int => {
                let b = read_inline(slot_abs, 8)?;
                Ok(Value::Int(i64::from_le_bytes(b.try_into().unwrap())))
            }
            TypeRepr::Float => {
                let b = read_inline(slot_abs, 8)?;
                Ok(Value::Float(ordered_float::OrderedFloat(
                    f64::from_le_bytes(b.try_into().unwrap()),
                )))
            }
            TypeRepr::Bool => {
                let b = read_inline(slot_abs, 1)?;
                Ok(Value::Bool(b[0] != 0))
            }
            TypeRepr::Unit => Ok(Value::option_none()),
            TypeRepr::String => {
                let ptr = self.read_slot_pointer(slot_abs)?;
                Ok(Value::String(self.read_string_record_at(ptr)?.into()))
            }
            TypeRepr::List { element } => {
                let header = self.read_slot_pointer(slot_abs)?;
                let vals = self.read_list_value_at(header, element)?;
                Ok(Value::List(std::sync::Arc::new(vals)))
            }
            TypeRepr::Schema { schema } => {
                let sub_layout = SchemaLayout::offsets_for(schema).map_err(|_| {
                    BufferError::MalformedPayload {
                        name: "<sub-record field>".to_string(),
                        reason: "nested schema field is not layoutable",
                    }
                })?;
                let sub_base = self.read_slot_pointer(slot_abs)?;
                self.read_record_at(sub_base, &sub_layout, schema)
            }
            TypeRepr::Option { .. } | TypeRepr::Result { .. } | TypeRepr::Enum { .. } => {
                let variant_base = self.read_slot_pointer(slot_abs)?;
                self.read_variant_record_at(variant_base, ty)
            }
            other => Err(BufferError::TypeMismatch {
                name: "<sub-record field>".to_string(),
                declared: type_label(other),
                requested: "scalar/String/List/Schema/Option/Result",
            }),
        }
    }

    fn read_variant_record_at(
        &self,
        record_off: usize,
        ty: &TypeRepr,
    ) -> Result<relon_eval_api::value::Value, BufferError> {
        use relon_eval_api::smol_str::SmolStr;
        use relon_eval_api::value::Value;
        if record_off
            .checked_add(1)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<variant>".to_string(),
                reason: "variant tag exceeds buffer end",
            });
        }
        let tag = self.bytes[record_off];
        let selected =
            variant_selected_payload(ty, tag).map_err(|reason| BufferError::MalformedPayload {
                name: "<variant>".to_string(),
                reason,
            })?;
        match ty {
            TypeRepr::Option { .. } => {
                match selected.payload {
                    None => Ok(Value::option_none()),
                    Some(payload) => {
                        let slot = variant_payload_slot_offset(record_off, &payload.ty)
                            .ok_or_else(|| BufferError::MalformedPayload {
                                name: "<variant>".to_string(),
                                reason: "variant payload slot offset overflows usize",
                            })?;
                        let value = self.read_field_at(slot, &payload.ty)?;
                        Ok(Value::option_some(value))
                    }
                }
            }
            TypeRepr::Result { .. } => {
                let Some(payload) = selected.payload else {
                    return Err(BufferError::MalformedPayload {
                        name: "<variant>".to_string(),
                        reason: "Result variant has no payload",
                    });
                };
                let slot =
                    variant_payload_slot_offset(record_off, &payload.ty).ok_or_else(|| {
                        BufferError::MalformedPayload {
                            name: "<variant>".to_string(),
                            reason: "variant payload slot offset overflows usize",
                        }
                    })?;
                let value = self.read_field_at(slot, &payload.ty)?;
                let mut map = std::collections::BTreeMap::new();
                map.insert(SmolStr::from(payload.key.unwrap_or("value")), value);
                Ok(Value::variant_dict(
                    map,
                    selected.name,
                    "Result".to_string(),
                ))
            }
            TypeRepr::Enum { name, .. } => {
                let mut map = std::collections::BTreeMap::new();
                if let Some(payload) = selected.payload {
                    let slot =
                        variant_payload_slot_offset(record_off, &payload.ty).ok_or_else(|| {
                            BufferError::MalformedPayload {
                                name: "<variant>".to_string(),
                                reason: "variant payload slot offset overflows usize",
                            }
                        })?;
                    let payload_value = self.read_field_at(slot, &payload.ty)?;
                    let Value::Dict(dict) = payload_value else {
                        return Err(BufferError::MalformedPayload {
                            name: "<variant>".to_string(),
                            reason: "enum payload did not decode to a record",
                        });
                    };
                    map = dict.map.clone();
                }
                Ok(Value::variant_dict(map, selected.name, name.clone()))
            }
            other => Err(BufferError::TypeMismatch {
                name: "<variant>".to_string(),
                declared: type_label(other),
                requested: "variant record",
            }),
        }
    }

    /// Read a `u32` pointer slot at the absolute offset `slot_abs`,
    /// bounds-checked, returning the pointed-at offset.
    fn read_slot_pointer(&self, slot_abs: usize) -> Result<usize, BufferError> {
        if slot_abs
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<sub-record field>".to_string(),
                reason: "pointer slot exceeds buffer end",
            });
        }
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.bytes[slot_abs..slot_abs + 4]);
        Ok(u32::from_le_bytes(b) as usize)
    }

    /// Read a `[len: u32][utf8]` String record at `record_off`,
    /// bounds-checked, validating UTF-8.
    fn read_string_record_at(&self, record_off: usize) -> Result<&'a str, BufferError> {
        if record_off
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<sub-record field>".to_string(),
                reason: "string len prefix exceeds buffer end",
            });
        }
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.bytes[record_off..record_off + 4]);
        let len = u32::from_le_bytes(b) as usize;
        let start = record_off + 4;
        let end = start
            .checked_add(len)
            .ok_or_else(|| BufferError::MalformedPayload {
                name: "<sub-record field>".to_string(),
                reason: "string payload end overflows usize",
            })?;
        if end > self.bytes.len() {
            return Err(BufferError::MalformedPayload {
                name: "<sub-record field>".to_string(),
                reason: "string payload exceeds buffer end",
            });
        }
        std::str::from_utf8(&self.bytes[start..end]).map_err(|_| BufferError::MalformedPayload {
            name: "<sub-record field>".to_string(),
            reason: "string payload is not valid utf-8",
        })
    }

    /// Read a `List<List<scalar>>` from `field_name` as a vector of
    /// inner `Vec<Value>`s. The outer slot points at a pointer-array
    /// header `[len][off_0]…[off_{N-1}]` whose entries name per-element
    /// inner records `[len: u32][pad to inner_align][payload]` — exactly
    /// what [`write_nested_scalar_list`] / `write_list_list_with` emit.
    /// Each inner record is decoded per the innermost scalar type
    /// (`Int` / `Float` / `Bool`); inner pointer-array elements
    /// (`List<List<String>>` / `List<List<Schema>>`) are rejected by the
    /// layout pass before any buffer is produced, so they never reach
    /// here and are surfaced as a hard error if they somehow do.
    ///
    /// The walk is the reader-side mirror of the writer and shares the
    /// same single buffer base: every `off_i` and inner `[len]` prefix is
    /// resolved against `self.bytes`, so a sub-reader produced by
    /// [`Self::read_list_record`] / [`Self::sub_record`] (which re-bases
    /// the field index but keeps the same `bytes` slice) decodes a nested
    /// list field bit-identically to a top-level one.
    pub fn read_list_list(
        &self,
        field_name: &str,
    ) -> Result<Vec<Vec<relon_eval_api::value::Value>>, BufferError> {
        let entry = self.find_entry(field_name)?;
        let inner = match &entry.ty {
            TypeRepr::List { element } => match element.as_ref() {
                TypeRepr::List { element: inner } => inner.as_ref().clone(),
                _ => {
                    return Err(BufferError::TypeMismatch {
                        name: field_name.to_string(),
                        declared: type_label(&entry.ty),
                        requested: "List<List<…>>",
                    });
                }
            },
            other => {
                return Err(BufferError::TypeMismatch {
                    name: field_name.to_string(),
                    declared: type_label(other),
                    requested: "List<List<…>>",
                });
            }
        };
        if !matches!(entry.kind, FieldKind::PointerIndirect { .. }) {
            return Err(BufferError::MalformedPayload {
                name: field_name.to_string(),
                reason: "expected pointer-indirect kind",
            });
        }
        // Per-inner-record alignment between the `[len]` prefix and the
        // payload, matching `write_nested_scalar_list`: Int / Float pad
        // the payload to 8, Bool packs at 1 (no pad past the 4-byte len).
        let inner_align: usize = match &inner {
            TypeRepr::Int | TypeRepr::Float => 8,
            TypeRepr::Bool => 1,
            other => {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: match other {
                        TypeRepr::String | TypeRepr::Schema { .. } | TypeRepr::List { .. } => {
                            "nested list inner element is a pointer-array type (unsupported)"
                        }
                        _ => "nested list inner element is not an inline-fixed scalar",
                    },
                });
            }
        };
        // Outer pointer-array header: `[len][off_0]…`. `inner_alignment
        // = 0` keeps the payload start at `header + 4` (the off array).
        let (count, entries_start) = self.decode_pointer_header(field_name, entry.offset, 0)?;
        self.decode_list_list_rows(field_name, &inner, inner_align, count, entries_start)
    }

    /// Decode a `List<List<scalar>>` value whose **outer pointer-array
    /// header** sits at `header_off` (a direct offset into `self.bytes`),
    /// rather than being reached through a record's fixed-area slot.
    ///
    /// This is the reader half of the in-place region-walk return ABI
    /// (S1). The machine code returns the arena-absolute offset of the
    /// root list header; the host rebases it to a region-relative offset,
    /// runs [`crate::verifier::verify_value_at`] over the whole reachable
    /// graph, and only then calls this to decode in place. `inner` is the
    /// innermost scalar element type (`Int` / `Float` / `Bool`) drained
    /// from the return layout. The decode shares
    /// [`Self::decode_list_list_rows`] with the field-slot
    /// [`Self::read_list_list`] path, so a top-level and an in-place
    /// decode of the same bytes are bit-identical.
    pub fn read_list_list_at(
        &self,
        header_off: usize,
        inner: &TypeRepr,
    ) -> Result<Vec<Vec<relon_eval_api::value::Value>>, BufferError> {
        let inner_align: usize = match inner {
            TypeRepr::Int | TypeRepr::Float => 8,
            TypeRepr::Bool => 1,
            other => {
                return Err(BufferError::MalformedPayload {
                    name: "<in-place root>".to_string(),
                    reason: match other {
                        TypeRepr::String | TypeRepr::Schema { .. } | TypeRepr::List { .. } => {
                            "nested list inner element is a pointer-array type (unsupported)"
                        }
                        _ => "nested list inner element is not an inline-fixed scalar",
                    },
                });
            }
        };
        // The header sits directly at `header_off`: `[len][off_0]…`.
        // `entries_start = header_off + 4`; bounds-check the length
        // prefix against the buffer end first.
        if header_off
            .checked_add(4)
            .map(|end| end > self.bytes.len())
            .unwrap_or(true)
        {
            return Err(BufferError::MalformedPayload {
                name: "<in-place root>".to_string(),
                reason: "in-place list header length prefix exceeds buffer end",
            });
        }
        let mut len_buf = [0u8; 4];
        len_buf.copy_from_slice(&self.bytes[header_off..header_off + 4]);
        let count = u32::from_le_bytes(len_buf) as usize;
        let entries_start = header_off + 4;
        self.decode_list_list_rows("<in-place root>", inner, inner_align, count, entries_start)
    }

    /// Shared row-decode for both the field-slot
    /// ([`Self::read_list_list`]) and direct-offset
    /// ([`Self::read_list_list_at`]) nested-list paths. Walks `count`
    /// pointer-array entries from `entries_start`, following each to its
    /// inner `[len][pad to inner_align][payload]` record and decoding the
    /// payload per the innermost scalar `inner` type. Every offset and
    /// length is bounds-checked against `self.bytes` before any read.
    fn decode_list_list_rows(
        &self,
        field_name: &str,
        inner: &TypeRepr,
        inner_align: usize,
        count: usize,
        entries_start: usize,
    ) -> Result<Vec<Vec<relon_eval_api::value::Value>>, BufferError> {
        use relon_eval_api::value::Value;
        self.check_pointer_entries_in_bounds(field_name, entries_start, count)?;
        let mut out: Vec<Vec<Value>> = Vec::with_capacity(count);
        for i in 0..count {
            let cursor =
                entries_start
                    .checked_add(i * 4)
                    .ok_or_else(|| BufferError::MalformedPayload {
                        name: field_name.to_string(),
                        reason: "nested list entry cursor overflows usize",
                    })?;
            if cursor + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "nested list entry pointer exceeds buffer end",
                });
            }
            let mut entry_buf = [0u8; 4];
            entry_buf.copy_from_slice(&self.bytes[cursor..cursor + 4]);
            let rec_start = u32::from_le_bytes(entry_buf) as usize;
            if rec_start + 4 > self.bytes.len() {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "nested list inner len prefix exceeds buffer end",
                });
            }
            let mut len_buf = [0u8; 4];
            len_buf.copy_from_slice(&self.bytes[rec_start..rec_start + 4]);
            let inner_count = u32::from_le_bytes(len_buf) as usize;
            let payload_start = if inner_align > 1 {
                (rec_start + 4).next_multiple_of(inner_align)
            } else {
                rec_start + 4
            };
            // Every inner scalar element occupies at least one byte, so a
            // valid `inner_count` can never exceed the bytes remaining
            // after `payload_start`. Bounding it here — before
            // `with_capacity` — stops a malformed header (`inner_count`
            // up to ~4.29e9) from requesting a multi-gigabyte allocation
            // and aborting the process; the per-arm exact `payload_end`
            // check below still enforces the tight element-width bound.
            if inner_count > self.bytes.len().saturating_sub(payload_start) {
                return Err(BufferError::MalformedPayload {
                    name: field_name.to_string(),
                    reason: "nested inner list count exceeds buffer end",
                });
            }
            let mut inner_vec: Vec<Value> = Vec::with_capacity(inner_count);
            match inner {
                TypeRepr::Int => {
                    let end = payload_start
                        .checked_add(inner_count.checked_mul(8).ok_or_else(|| {
                            BufferError::MalformedPayload {
                                name: field_name.to_string(),
                                reason: "nested Int payload byte length overflows usize",
                            }
                        })?)
                        .ok_or_else(|| BufferError::MalformedPayload {
                            name: field_name.to_string(),
                            reason: "nested Int payload end overflows usize",
                        })?;
                    if end > self.bytes.len() {
                        return Err(BufferError::MalformedPayload {
                            name: field_name.to_string(),
                            reason: "nested Int payload exceeds buffer end",
                        });
                    }
                    let mut c = payload_start;
                    for _ in 0..inner_count {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&self.bytes[c..c + 8]);
                        inner_vec.push(Value::Int(i64::from_le_bytes(b)));
                        c += 8;
                    }
                }
                TypeRepr::Float => {
                    let end = payload_start
                        .checked_add(inner_count.checked_mul(8).ok_or_else(|| {
                            BufferError::MalformedPayload {
                                name: field_name.to_string(),
                                reason: "nested Float payload byte length overflows usize",
                            }
                        })?)
                        .ok_or_else(|| BufferError::MalformedPayload {
                            name: field_name.to_string(),
                            reason: "nested Float payload end overflows usize",
                        })?;
                    if end > self.bytes.len() {
                        return Err(BufferError::MalformedPayload {
                            name: field_name.to_string(),
                            reason: "nested Float payload exceeds buffer end",
                        });
                    }
                    let mut c = payload_start;
                    for _ in 0..inner_count {
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&self.bytes[c..c + 8]);
                        inner_vec.push(Value::Float(ordered_float::OrderedFloat(
                            f64::from_le_bytes(b),
                        )));
                        c += 8;
                    }
                }
                TypeRepr::Bool => {
                    let end = payload_start.checked_add(inner_count).ok_or_else(|| {
                        BufferError::MalformedPayload {
                            name: field_name.to_string(),
                            reason: "nested Bool payload end overflows usize",
                        }
                    })?;
                    if end > self.bytes.len() {
                        return Err(BufferError::MalformedPayload {
                            name: field_name.to_string(),
                            reason: "nested Bool payload exceeds buffer end",
                        });
                    }
                    for k in 0..inner_count {
                        inner_vec.push(Value::Bool(self.bytes[payload_start + k] != 0));
                    }
                }
                _ => unreachable!("inner_align guard already rejected non-scalar inner"),
            }
            out.push(inner_vec);
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
        | (TypeRepr::Unit, TypeRepr::Unit)
        | (TypeRepr::String, TypeRepr::String) => true,
        (TypeRepr::List { element: d }, TypeRepr::List { element: r }) => {
            type_matches(d.as_ref(), r.as_ref())
        }
        (TypeRepr::Option { inner: d }, TypeRepr::Option { inner: r }) => {
            type_matches(d.as_ref(), r.as_ref())
        }
        (TypeRepr::Result { ok: dok, err: derr }, TypeRepr::Result { ok: rok, err: rerr }) => {
            type_matches(dok.as_ref(), rok.as_ref()) && type_matches(derr.as_ref(), rerr.as_ref())
        }
        (TypeRepr::Schema { schema: d }, TypeRepr::Schema { schema: r }) => d == r,
        (
            TypeRepr::Enum {
                name: dn,
                variants: dv,
            },
            TypeRepr::Enum {
                name: rn,
                variants: rv,
            },
        ) => dn == rn && dv == rv,
        _ => false,
    }
}

/// Human-readable label for the schema's declared type. Used in the
/// `TypeMismatch` error path so users see "Int" rather than `Debug`-
/// formatted `TypeRepr::Int`.
fn type_label(ty: &TypeRepr) -> &'static str {
    match ty {
        TypeRepr::Unit => "Unit",
        TypeRepr::Bool => "Bool",
        TypeRepr::Int => "Int",
        TypeRepr::Float => "Float",
        TypeRepr::String => "String",
        TypeRepr::List { .. } => "List",
        TypeRepr::Option { .. } => "Option",
        TypeRepr::Result { .. } => "Result",
        TypeRepr::Enum { .. } => "Enum",
        TypeRepr::Schema { .. } => "Schema",
        TypeRepr::Closure { .. } => "Closure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::SchemaLayout;
    use crate::schema_canonical::{Field, Schema};
    use relon_eval_api::value::Value;

    fn field(name: &str, ty: TypeRepr) -> Field {
        Field {
            name: name.into(),
            ty,
            default: None,
        }
    }

    fn result_ok(value: Value) -> Value {
        let mut map = std::collections::BTreeMap::new();
        map.insert(relon_eval_api::smol_str::SmolStr::from("value"), value);
        Value::variant_dict(map, "Ok".to_string(), "Result".to_string())
    }

    fn result_err(value: Value) -> Value {
        let mut map = std::collections::BTreeMap::new();
        map.insert(relon_eval_api::smol_str::SmolStr::from("error"), value);
        Value::variant_dict(map, "Err".to_string(), "Result".to_string())
    }

    #[test]
    fn option_and_result_fields_roundtrip_through_buffer_and_verifier() {
        let schema = Schema {
            name: "Variants".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![
                field(
                    "maybe",
                    TypeRepr::Option {
                        inner: Box::new(TypeRepr::Int),
                    },
                ),
                field(
                    "res",
                    TypeRepr::Result {
                        ok: Box::new(TypeRepr::Int),
                        err: Box::new(TypeRepr::String),
                    },
                ),
                field(
                    "ok",
                    TypeRepr::Result {
                        ok: Box::new(TypeRepr::Int),
                        err: Box::new(TypeRepr::String),
                    },
                ),
            ],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        let maybe = Value::option_some(Value::Int(42));
        let res = result_err(Value::String("bad".into()));
        let ok = result_ok(Value::Int(7));
        builder
            .write_value("maybe", &schema.fields[0].ty, &maybe)
            .expect("write option");
        builder
            .write_value("res", &schema.fields[1].ty, &res)
            .expect("write result err");
        builder
            .write_value("ok", &schema.fields[2].ty, &ok)
            .expect("write result ok");
        let bytes = builder.finish();
        crate::verifier::verify_record(
            &bytes,
            &layout,
            &schema.fields,
            0,
            crate::verifier::Region::new(0, bytes.len()).unwrap(),
        )
        .expect("verify");
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(
            reader.read_value("maybe", &schema.fields[0].ty).unwrap(),
            maybe
        );
        assert_eq!(reader.read_value("res", &schema.fields[1].ty).unwrap(), res);
    }

    #[test]
    fn option_string_inside_nested_schema_relocates_when_pasted() {
        let inner = Schema {
            name: "Inner".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field(
                "maybe",
                TypeRepr::Option {
                    inner: Box::new(TypeRepr::String),
                },
            )],
        };
        let outer = Schema {
            name: "Outer".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field(
                "inner",
                TypeRepr::Schema {
                    schema: Box::new(inner.clone()),
                },
            )],
        };
        let layout = SchemaLayout::offsets_for(&outer).expect("layout");
        let mut map = std::collections::BTreeMap::new();
        let payload = Value::option_some(Value::String("hello".into()));
        map.insert(
            relon_eval_api::smol_str::SmolStr::from("maybe"),
            payload.clone(),
        );
        let value = Value::branded_dict(map, Some("Inner".to_string()));
        let mut builder = BufferBuilder::new(&layout, &outer.fields);
        builder
            .write_value("inner", &outer.fields[0].ty, &value)
            .expect("write nested schema");
        let bytes = builder.finish_arena_absolute(64).expect("arena rebase");
        let mut arena = vec![0u8; 64];
        arena.extend_from_slice(&bytes);
        let reader = BufferReader::new_at_base(&layout, &outer.fields, &arena, 64).expect("reader");
        let decoded = reader.read_value("inner", &outer.fields[0].ty).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn list_option_string_relocates_variant_entry_payloads() {
        let schema = Schema {
            name: "Rows".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field(
                "xs",
                TypeRepr::List {
                    element: Box::new(TypeRepr::Option {
                        inner: Box::new(TypeRepr::String),
                    }),
                },
            )],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let value = Value::list(vec![
            Value::option_some(Value::String("a".into())),
            Value::option_none(),
            Value::option_some(Value::String("bc".into())),
        ]);
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder
            .write_value("xs", &schema.fields[0].ty, &value)
            .expect("write list option");
        let bytes = builder.finish_arena_absolute(128).expect("arena rebase");
        let mut arena = vec![0u8; 128];
        arena.extend_from_slice(&bytes);
        crate::verifier::verify_record_multi(
            &arena,
            &layout,
            &schema.fields,
            128,
            crate::verifier::MultiRegion::new(
                (0, 128),
                (128, arena.len()),
                (arena.len(), arena.len()),
                (arena.len(), arena.len()),
            )
            .unwrap(),
        )
        .expect("verify arena absolute");
        let reader =
            BufferReader::new_at_base(&layout, &schema.fields, &arena, 128).expect("reader");
        assert_eq!(
            reader.read_value("xs", &schema.fields[0].ty).unwrap(),
            value
        );
    }

    fn int_schema() -> Schema {
        Schema {
            name: "Pair".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field("x", TypeRepr::Int), field("y", TypeRepr::Int)],
        }
    }

    fn mixed_schema() -> Schema {
        Schema {
            name: "Mix".into(),
            generics: vec![],
            is_tuple: false,
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
    fn float_and_unit_roundtrip() {
        let schema = Schema {
            name: "Phys".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field("mass", TypeRepr::Float), field("nil", TypeRepr::Unit)],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut builder = BufferBuilder::new(&layout, &schema.fields);
        builder.write_float("mass", 1.5_f64).expect("write mass");
        builder.write_unit("nil").expect("write nil");
        let bytes = builder.finish();
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        assert_eq!(reader.read_float("mass").expect("read mass"), 1.5);
        reader.read_unit("nil").expect("read nil");
    }

    #[test]
    fn write_string_then_read_back_roundtrips() {
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            is_tuple: false,
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
    /// by `docs/internal/archive/review-improvement-169-conststring-wire-full-2026-05-22.md`
    /// must update producer + every consumer atomically; this test
    /// fires the moment the producer side drifts so the migrant cannot
    /// silently land a partial revision.
    ///
    /// See also `relon-codegen-cranelift::codegen::const_pool::
    /// opvisitor_emits_const_string_record_in_declaration_order` for
    /// the matching pin on the cranelift const-pool producer.
    #[test]
    fn write_string_wire_format_smoke_gate() {
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("city", TypeRepr::String), field("zip", TypeRepr::Int)],
        };
        let usr_schema = Schema {
            name: "Usr".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("s", TypeRepr::String), field("i", TypeRepr::Int)],
        };
        let outer = Schema {
            name: "Outer".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("age", TypeRepr::Int)],
        };
        let wrap_schema = Schema {
            name: "Wrap".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("city", TypeRepr::String), field("zip", TypeRepr::Int)],
        };
        let usr_schema = Schema {
            name: "Usr".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("tag", TypeRepr::String)],
        };
        let outer_schema = Schema {
            name: "Outer".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("x", TypeRepr::Int)],
        };
        let outer_schema = Schema {
            name: "Outer".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("x", TypeRepr::Int)],
        };
        let mixed = Schema {
            name: "Mixed".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
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

    /// Regression (Medium DoS): a pointer-array / record list header whose
    /// declared `count` is far larger than the buffer can hold must be a
    /// loud `Err`, never a multi-gigabyte `Vec::with_capacity` that aborts
    /// the process. Before the fix, `count` (an untrusted `u32`, here
    /// `0xFFFF_FFFF`) was fed straight into `with_capacity` — requesting
    /// ~17 GiB of entry slots — ahead of the per-entry bounds checks.
    #[test]
    fn oversized_list_count_is_loud_error_not_oom() {
        // --- Direct-offset (`*_at`) readers: craft a tiny buffer whose
        // header at offset 0 declares count = u32::MAX. ---
        let holder = list_schema("xs", TypeRepr::String);
        let holder_layout = SchemaLayout::offsets_for(&holder).expect("layout");
        let mut bytes = vec![0u8; 32];
        bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let reader = BufferReader::new(&holder_layout, &holder.fields, &bytes).expect("reader");

        let elem_schema = Schema {
            name: "Elem".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field("n", TypeRepr::Int)],
        };
        let elem_layout = SchemaLayout::offsets_for(&elem_schema).expect("elem layout");

        assert!(
            reader.read_list_string_at(0).is_err(),
            "read_list_string_at must reject oversized count"
        );
        assert!(
            reader
                .read_list_record_at(0, &elem_layout, &elem_schema)
                .is_err(),
            "read_list_record_at must reject oversized count"
        );
        assert!(
            reader
                .read_list_value_at(
                    0,
                    &TypeRepr::Schema {
                        schema: Box::new(elem_schema.clone()),
                    },
                )
                .is_err(),
            "read_list_value_at (pointer-array) must reject oversized count"
        );
        assert!(
            reader.read_list_list_at(0, &TypeRepr::Int).is_err(),
            "read_list_list_at must reject oversized outer count"
        );

        // --- Field-slot readers: build a *valid* List<String>, confirm it
        // decodes bit-equal, then corrupt only its length prefix and
        // confirm the reader now errors instead of over-allocating. ---
        let schema = list_schema("xs", TypeRepr::String);
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        b.write_list_string("xs", &["a", "bb", "ccc"])
            .expect("write");
        let mut buf = b.finish();
        {
            let r = BufferReader::new(&layout, &schema.fields, &buf).expect("reader");
            assert_eq!(
                r.read_list_string("xs").expect("valid decode"),
                vec!["a", "bb", "ccc"],
                "valid list must still decode bit-equal"
            );
        }
        // Field slot for the single field sits at offset 0 and holds the
        // record start; the `[len]` prefix is the first u32 of that record.
        let record_start = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        buf[record_start..record_start + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let r = BufferReader::new(&layout, &schema.fields, &buf).expect("reader");
        assert!(
            r.read_list_string("xs").is_err(),
            "corrupted oversized count must be a loud error"
        );

        // Same corruption exercised through the List<Schema> record reader.
        let rec_schema = Schema {
            name: "Wrap".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field(
                "items",
                TypeRepr::List {
                    element: Box::new(TypeRepr::Schema {
                        schema: Box::new(elem_schema.clone()),
                    }),
                },
            )],
        };
        let rec_layout = SchemaLayout::offsets_for(&rec_schema).expect("rec layout");
        let mut rb = BufferBuilder::new(&rec_layout, &rec_schema.fields);
        let mk = |val: i64| move |w: &mut BufferBuilder<'_>| w.write_int("n", val);
        let entries = [mk(1), mk(2)];
        rb.write_list_record("items", &elem_layout, &elem_schema, &entries)
            .expect("write records");
        let mut rbuf = rb.finish();
        let rec_ptr = u32::from_le_bytes(rbuf[0..4].try_into().unwrap()) as usize;
        rbuf[rec_ptr..rec_ptr + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let rr = BufferReader::new(&rec_layout, &rec_schema.fields, &rbuf).expect("reader");
        assert!(
            rr.read_list_record("items", &elem_layout, &elem_schema)
                .is_err(),
            "corrupted oversized record count must be a loud error"
        );
    }

    #[test]
    fn mixed_record_and_list_string_roundtrip() {
        // Phase 10-c: a top-level Int + a List<String> share the tail
        // area. Verifies the cursor advances correctly across heterogeneous
        // tail appends.
        let schema = Schema {
            name: "Mixed".into(),
            generics: vec![],
            is_tuple: false,
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
            is_tuple: false,
            fields: vec![field("name", TypeRepr::String), field("age", TypeRepr::Int)],
        };
        let outer = Schema {
            name: "Group".into(),
            generics: vec![],
            is_tuple: false,
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

    /// Nested `List<List<Int>>`: the header's `[len][off_i]` pointer
    /// array names per-element inner `[len][pad][i64...]` records.
    /// Decodes the buffer by hand (no reader API for nested lists yet)
    /// to pin the exact byte layout the compiled backends consume.
    #[test]
    fn write_nested_scalar_list_int_layout() {
        use relon_eval_api::value::Value;
        let schema = Schema {
            name: "Grid".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field(
                "xss",
                TypeRepr::List {
                    element: Box::new(TypeRepr::List {
                        element: Box::new(TypeRepr::Int),
                    }),
                },
            )],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("layout");
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        let items = vec![
            Value::List(vec![Value::Int(1), Value::Int(2)].into()),
            Value::List(vec![Value::Int(3)].into()),
            Value::List(vec![].into()),
        ];
        write_nested_scalar_list(&mut b, "xss", &TypeRepr::Int, &items).expect("write");
        let bytes = b.finish();

        // Field slot at offset 0 → header offset.
        let read_u32 =
            |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let header = read_u32(0);
        assert_eq!(read_u32(header), 3, "outer len");
        // Each inner record: [len:u32][pad to 8][i64 x len].
        let decode_inner = |rec: usize| -> Vec<i64> {
            let n = read_u32(rec);
            let payload = (rec + 4).next_multiple_of(8);
            (0..n)
                .map(|i| {
                    let at = payload + i * 8;
                    i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
                })
                .collect()
        };
        let off0 = read_u32(header + 4);
        let off1 = read_u32(header + 8);
        let off2 = read_u32(header + 12);
        assert_eq!(decode_inner(off0), vec![1, 2]);
        assert_eq!(decode_inner(off1), vec![3]);
        assert_eq!(decode_inner(off2), Vec::<i64>::new());
    }

    /// `read_list_list` is the reader-side mirror of
    /// `write_nested_scalar_list`: round-tripping any nested scalar list
    /// through the writer and back yields the exact input rows. The
    /// written `Value`s are the oracle, so this pins the host walk
    /// bit-for-bit independent of any compiled backend.
    #[test]
    fn read_list_list_roundtrips_nested_scalars() {
        use relon_eval_api::value::Value;
        let cases: &[(TypeRepr, Vec<Value>)] = &[
            (
                TypeRepr::Int,
                vec![
                    Value::List(vec![Value::Int(1), Value::Int(2)].into()),
                    Value::List(vec![Value::Int(-7)].into()),
                    Value::List(vec![].into()),
                    Value::List(vec![Value::Int(i64::MAX), Value::Int(i64::MIN)].into()),
                ],
            ),
            (
                TypeRepr::Float,
                vec![
                    Value::List(
                        vec![
                            Value::Float(ordered_float::OrderedFloat(1.5)),
                            Value::Float(ordered_float::OrderedFloat(-0.25)),
                        ]
                        .into(),
                    ),
                    Value::List(vec![].into()),
                ],
            ),
            (
                TypeRepr::Bool,
                vec![
                    Value::List(
                        vec![Value::Bool(true), Value::Bool(false), Value::Bool(true)].into(),
                    ),
                    Value::List(vec![Value::Bool(false)].into()),
                ],
            ),
        ];
        for (inner, items) in cases {
            let schema = Schema {
                name: "Grid".into(),
                generics: vec![],
                is_tuple: false,
                fields: vec![field(
                    "xss",
                    TypeRepr::List {
                        element: Box::new(TypeRepr::List {
                            element: Box::new(inner.clone()),
                        }),
                    },
                )],
            };
            let layout = SchemaLayout::offsets_for(&schema).expect("layout");
            let mut b = BufferBuilder::new(&layout, &schema.fields);
            write_nested_scalar_list(&mut b, "xss", inner, items).expect("write");
            let bytes = b.finish();
            let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
            let rows = reader.read_list_list("xss").expect("read_list_list");
            let got: Vec<Value> = rows.into_iter().map(|r| Value::List(r.into())).collect();
            assert_eq!(&got, items, "nested {inner:?} list roundtrip mismatch");
        }
    }

    /// F5: `write_nested_pointer_array_list` marshals a `List<List<String>>`
    /// and `read_list_value` decodes it back bit-identically, including
    /// empty inner / outer lists and an empty / multibyte string. The
    /// doubly-nested pointer array round-trips through one buffer.
    #[test]
    fn nested_list_inner_string_roundtrips() {
        use relon_eval_api::value::Value;
        let inner_str = TypeRepr::List {
            element: Box::new(TypeRepr::String),
        };
        let schema = Schema {
            name: "Grid".into(),
            generics: vec![],
            is_tuple: false,
            fields: vec![field(
                "xss",
                TypeRepr::List {
                    element: Box::new(inner_str.clone()),
                },
            )],
        };
        let layout = SchemaLayout::offsets_for(&schema).expect("List<List<String>> layout");
        let multibyte: String = [0x4E2Du32, 0x6587]
            .iter()
            .map(|c| char::from_u32(*c).unwrap())
            .collect();
        let rows: Vec<Value> = vec![
            Value::List(std::sync::Arc::new(vec![
                Value::String("a".into()),
                Value::String("".into()),
                Value::String(multibyte.as_str().into()),
            ])),
            Value::List(std::sync::Arc::new(vec![])),
            Value::List(std::sync::Arc::new(vec![Value::String("zz".into())])),
        ];
        let mut b = BufferBuilder::new(&layout, &schema.fields);
        // The marshaller's `element` is the *innermost* element (String),
        // mirroring the `marshal_list_list_in` dispatch contract.
        write_nested_pointer_array_list(&mut b, "xss", &TypeRepr::String, &rows)
            .expect("write nested");
        let bytes = b.finish();
        // Read it back through the field slot (the reader's `element` is
        // the *outer* list element, `List<String>`), then via the in-place
        // root.
        let reader = BufferReader::new(&layout, &schema.fields, &bytes).expect("reader");
        let got = reader
            .read_list_value("xss", &inner_str)
            .expect("read field");
        assert_eq!(
            Value::List(std::sync::Arc::new(got)),
            Value::List(std::sync::Arc::new(rows.clone()))
        );
        // And via the direct header offset (the in-place return path).
        let fo = &layout.fields[0];
        let mut slot = [0u8; 4];
        slot.copy_from_slice(&bytes[fo.offset..fo.offset + 4]);
        let header = u32::from_le_bytes(slot) as usize;
        let got2 = reader
            .read_list_value_at(header, &inner_str)
            .expect("read at header");
        assert_eq!(
            Value::List(std::sync::Arc::new(got2)),
            Value::List(std::sync::Arc::new(rows))
        );
    }
}
