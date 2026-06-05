//! Static field-offset table for the host <-> wasm binary handshake.
//!
//! Spec: `docs/internal/wasm-binary-layout-v1-2026-05-16.md`,
//! "basic type layout" + "slot alignment rule" sections.
//!
//! v1 scope (Phase 2.a / 2.b / 2.c):
//!
//! * Phase 2.a / 2.b: the four scalar leaves — `Int`, `Float`, `Bool`,
//!   `Null` — are laid out inline. Each field's start offset is rounded
//!   up to its alignment; the root is padded to `root_align` so the
//!   next record starts aligned.
//! * Phase 2.c: `String` and `List<Int>` join via pointer indirection.
//!   Each variable field contributes a fixed-area `u32` pointer slot
//!   (size = 4, align = 4); the actual `[len][bytes...]` payload lives
//!   in a tail area the [`crate::buffer::BufferBuilder`] appends after
//!   the root record. Other variable-length leaves (`Dict`, `Option`,
//!   `Result`, nested schemas, lists of non-`Int` element types) are
//!   still rejected via [`LayoutError::UnsupportedTypeInLayoutV1`].

use crate::schema_canonical::{Schema, TypeRepr};
use thiserror::Error;

/// How a field is stored relative to the buffer record.
///
/// Phase 2.b only emitted `Inline` slots (the v1 scalar leaves). Phase
/// 2.c adds `PointerIndirect` for `String` / `List<Int>`: the fixed
/// area holds a `u32` pointer to a `[len: u32 LE][payload...]` record
/// appended in the tail area by [`crate::buffer::BufferBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Value is stored directly in the fixed area. `size`/`align` on
    /// the enclosing [`FieldOffset`] describe the slot. All v1 scalar
    /// leaves use this shape.
    Inline {
        /// Slot size in bytes (excluding any trailing padding).
        size: usize,
        /// Required alignment in bytes.
        align: usize,
    },
    /// Fixed area holds a 4-byte (aligned to 4) `u32` pointer to a
    /// tail-area record. The tail record's leading `[len: u32 LE]`
    /// covers byte / element count; element alignment is encoded on
    /// the variant so [`crate::buffer::BufferBuilder`] can pad the
    /// tail-area cursor before appending the payload.
    PointerIndirect {
        /// Required alignment for the tail-area payload, in bytes.
        /// `String` uses `1` (raw bytes); `List<Int>` uses `8` (i64
        /// elements). The length prefix itself is always 4-byte
        /// aligned by the builder.
        tail_alignment: usize,
    },
}

/// Phase 10-c: per-element layout descriptor for a `List<T>` field.
///
/// Carried as a sidecar on [`FieldOffset`] for any pointer-indirect
/// slot whose declared type is a `List<T>`. The buffer writer / reader
/// dispatches on this to pick the right tail-area shape:
///
/// * `InlineFixed { elem_size, elem_align }` — fixed-stride payload
///   laid out as `[len: u32][padding to elem_align][elem_0][elem_1]...`.
///   Used by `List<Int>` (`8/8`), `List<Float>` (`8/8`) and
///   `List<Bool>` (`1/1`, no inter-element padding per spec).
/// * `PointerArray { elem_alignment }` — variable-stride payload laid
///   out as `[len: u32][off_0: u32][off_1: u32]...` followed by each
///   element's own tail record in the same buffer. `elem_alignment` is
///   the alignment the inner records demand when emitted (`4` for
///   `String` len-prefixes; `root_align` for sub-record fixed areas).
///   Used by `List<String>` and `List<branded Schema>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListElementKind {
    /// Fixed-size inline elements. The list record's shape is
    /// `[len: u32][pad to elem_align][elem_0 ..]`; the pad is `0`
    /// bytes for 1-aligned elements (booleans) and `4` bytes for
    /// 8-aligned elements (i64 / f64).
    InlineFixed {
        /// Element size in bytes.
        elem_size: usize,
        /// Element alignment in bytes. Drives the post-`len` padding
        /// the builder inserts before the first element.
        elem_align: usize,
    },
    /// Variable-size elements addressed through a buffer-relative
    /// `u32` pointer array immediately after the `len` prefix.
    PointerArray {
        /// Alignment required by each inner element record when the
        /// builder emits it in the tail area. `String` elements pass
        /// `4` (len-prefix alignment); branded `Schema` elements pass
        /// the sub-schema's `root_align`.
        elem_alignment: usize,
    },
}

/// One field's slot inside a record.
///
/// `offset` is the byte position relative to the buffer base. For
/// `Inline` slots `size` / `align` reflect the leaf type. For
/// `PointerIndirect` slots the fixed-area pointer is always 4 bytes
/// at 4-byte alignment; the `kind` carries the tail-area alignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldOffset {
    /// Field name as declared in the source schema.
    pub name: String,
    /// Byte offset from the record base. Always a multiple of the
    /// slot's effective alignment (either `kind`'s align for `Inline`
    /// or `4` for `PointerIndirect`).
    pub offset: usize,
    /// Fixed-area slot size in bytes (excluding any trailing padding
    /// to `root_align`). For `PointerIndirect` slots this is always
    /// `4` — the pointer width — even though the tail-area payload
    /// can be arbitrarily long.
    pub size: usize,
    /// Required alignment of this slot's start in bytes. Mirrors the
    /// `kind` alignment for `Inline`; `4` for `PointerIndirect`.
    pub align: usize,
    /// How the field's payload reaches the wasm side — directly in
    /// the fixed area (`Inline`), or through a tail-area pointer
    /// (`PointerIndirect`).
    pub kind: FieldKind,
    /// Phase 10-c: per-element layout for `List<T>` fields. `None` for
    /// non-list fields; `Some` when the field's declared `TypeRepr` is
    /// `List<T>` and the v1 layout supports `T`. Carried alongside
    /// [`FieldKind`] so the buffer writer / reader can dispatch on the
    /// element shape without re-walking the schema.
    pub list_element: Option<ListElementKind>,
}

/// Computed offset table for a schema's flat record area.
///
/// `root_size` covers the **fixed area only** — the per-field slot
/// (inline payload or pointer) plus padding to `root_align`. Variable
/// payloads (Phase 2.c `String` / `List<Int>`) live in a tail area
/// the [`crate::buffer::BufferBuilder`] appends *after* `root_size`
/// bytes; the wasm side reaches them through the pointer slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetTable {
    /// Field slots in declaration order.
    pub fields: Vec<FieldOffset>,
    /// Total fixed record area size, padded to `root_align`.
    pub root_size: usize,
    /// Maximum alignment required by any field. For an empty schema
    /// this defaults to `1` so callers can still allocate a zero-size
    /// buffer with a benign alignment hint.
    pub root_align: usize,
}

impl OffsetTable {
    /// Alias for `root_size` that documents the Phase 2.c split — the
    /// returned value covers only the fixed (root) area, not any
    /// `String` / `List<Int>` tail-area bytes the builder appends.
    pub fn fixed_area_size(&self) -> usize {
        self.root_size
    }

    /// `true` when at least one field is `PointerIndirect`, meaning
    /// the builder needs a tail area beyond `root_size`. Lets the
    /// codegen pass skip the tail-cursor bookkeeping for purely
    /// scalar schemas.
    pub fn requires_tail_area(&self) -> bool {
        self.fields
            .iter()
            .any(|f| matches!(f.kind, FieldKind::PointerIndirect { .. }))
    }
}

/// Reasons offset computation can fail.
///
/// All variants surface at codegen / host-prep time, never at
/// runtime; the binary layout is fully static.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// The schema references a type the v1 layout does not yet model.
    /// String / List / Dict / Option / Result / Enum live in this
    /// bucket until Phase 2.b plumbs pointer-indirection in.
    #[error("layout v1 does not yet support type `{kind}` in field `{field}`")]
    UnsupportedTypeInLayoutV1 {
        /// Field name that triggered the error.
        field: String,
        /// Human-readable type kind (`"String"`, `"List"`, ...).
        kind: &'static str,
    },
    /// Phase 10-c: `List<T>` with an element type the v1 layout
    /// declines. Currently covers `List<List<...>>`, `List<Option<T>>`,
    /// and `List<Result<T, E>>`. The error spells out the inner kind
    /// so the host SDK can surface a precise message.
    #[error("layout v1 does not yet support list element `{inner}` in field `{field}`")]
    UnsupportedListElement {
        /// Field name that triggered the error.
        field: String,
        /// Human-readable name of the inner type.
        inner: &'static str,
    },
    /// Cumulative offset overflowed `usize`. Astronomically unlikely
    /// on 64-bit hosts but cheap to model so the layout pass never
    /// quietly wraps.
    #[error("layout overflow while placing field `{field}` at offset {offset}")]
    Overflow {
        /// Field name being placed when overflow happened.
        field: String,
        /// Last successfully computed offset before overflow.
        offset: usize,
    },
}

/// Per-field layout decision. Carries the fixed-area slot description
/// plus the optional element-shape sidecar `List<T>` fields need.
struct FieldLayoutDecision {
    kind: FieldKind,
    list_element: Option<ListElementKind>,
}

/// Compute the field-kind / fixed-area placement for one [`TypeRepr`].
///
/// Returns `Ok(decision)` on success. `Err(label)` carries the human-
/// readable kind label for [`LayoutError::UnsupportedTypeInLayoutV1`]
/// when the v1 layout still declines the type at the field level
/// (`Option`, `Result`). `List<T>` element rejection routes through
/// a different error variant ([`LayoutError::UnsupportedListElement`])
/// — those failures bubble up from this function as their own error,
/// returned in the `Err` arm tagged with the `"List"` label and a
/// nested cause via the field name (caller assembles the precise
/// message).
///
/// Supported set:
///
/// * `Null`, `Bool` → `Inline { size: 1, align: 1 }`.
/// * `Int`, `Float` → `Inline { size: 8, align: 8 }`.
/// * `String` → `PointerIndirect { tail_alignment: 1 }`.
/// * `List<Int>` / `List<Float>` → `PointerIndirect { tail_alignment: 8 }`
///   with `InlineFixed { elem_size: 8, elem_align: 8 }`.
/// * `List<Bool>` → `PointerIndirect { tail_alignment: 4 }` with
///   `InlineFixed { elem_size: 1, elem_align: 1 }` (booleans pack
///   tightly per spec, no inter-element padding).
/// * `List<String>` → `PointerIndirect { tail_alignment: 4 }` with
///   `PointerArray { elem_alignment: 4 }`.
/// * `List<Schema>` → `PointerIndirect { tail_alignment: 4 }` with
///   `PointerArray { elem_alignment: sub.root_align }`.
/// * `Schema { ... }` → `PointerIndirect { tail_alignment: sub.root_align }`.
fn field_layout_decision_for(
    field_name: &str,
    ty: &TypeRepr,
) -> Result<FieldLayoutDecision, LayoutError> {
    match ty {
        TypeRepr::Null => Ok(FieldLayoutDecision {
            kind: FieldKind::Inline { size: 1, align: 1 },
            list_element: None,
        }),
        TypeRepr::Bool => Ok(FieldLayoutDecision {
            kind: FieldKind::Inline { size: 1, align: 1 },
            list_element: None,
        }),
        TypeRepr::Int => Ok(FieldLayoutDecision {
            kind: FieldKind::Inline { size: 8, align: 8 },
            list_element: None,
        }),
        TypeRepr::Float => Ok(FieldLayoutDecision {
            kind: FieldKind::Inline { size: 8, align: 8 },
            list_element: None,
        }),
        TypeRepr::String => Ok(FieldLayoutDecision {
            kind: FieldKind::PointerIndirect { tail_alignment: 1 },
            list_element: None,
        }),
        TypeRepr::List { element } => list_layout_decision(field_name, element.as_ref()),
        TypeRepr::Schema { schema } => {
            // Recursively compute the sub-record's root_align so the
            // tail-area placement of the sub-record honours its own
            // alignment requirements (an inner i64 field demands the
            // sub-record start on an 8-byte boundary).
            let sub = SchemaLayout::offsets_for(schema)?;
            Ok(FieldLayoutDecision {
                kind: FieldKind::PointerIndirect {
                    tail_alignment: sub.root_align,
                },
                list_element: None,
            })
        }
        TypeRepr::Option { .. } => Err(LayoutError::UnsupportedTypeInLayoutV1 {
            field: field_name.to_string(),
            kind: "Option",
        }),
        TypeRepr::Result { .. } => Err(LayoutError::UnsupportedTypeInLayoutV1 {
            field: field_name.to_string(),
            kind: "Result",
        }),
        // Phase F.2: closure fields have no host-visible binary layout
        // — the runtime handle is a scratch-heap pointer that doesn't
        // survive the `run_main` boundary. Reject here so the binary
        // handshake builder doesn't paper over a dangling pointer.
        TypeRepr::Closure { .. } => Err(LayoutError::UnsupportedTypeInLayoutV1 {
            field: field_name.to_string(),
            kind: "Closure",
        }),
    }
}

/// Decide layout for a `List<element>` field. v1 supports the
/// fixed-stride scalar elements (`Int`, `Float`, `Bool`) inline and
/// the variable-stride elements (`String`, branded `Schema`) through
/// a per-element pointer array. Other element shapes route to
/// [`LayoutError::UnsupportedListElement`].
fn list_layout_decision(
    field_name: &str,
    element: &TypeRepr,
) -> Result<FieldLayoutDecision, LayoutError> {
    match element {
        TypeRepr::Int | TypeRepr::Float => Ok(FieldLayoutDecision {
            kind: FieldKind::PointerIndirect { tail_alignment: 8 },
            list_element: Some(ListElementKind::InlineFixed {
                elem_size: 8,
                elem_align: 8,
            }),
        }),
        TypeRepr::Bool => Ok(FieldLayoutDecision {
            // Bool elements are 1-aligned; the record still needs a
            // 4-byte aligned start so the `[len:u32]` prefix is
            // naturally aligned. Builder pads to 4 before writing the
            // record, then writes the len + booleans tightly per spec.
            kind: FieldKind::PointerIndirect { tail_alignment: 4 },
            list_element: Some(ListElementKind::InlineFixed {
                elem_size: 1,
                elem_align: 1,
            }),
        }),
        TypeRepr::String => Ok(FieldLayoutDecision {
            kind: FieldKind::PointerIndirect { tail_alignment: 4 },
            list_element: Some(ListElementKind::PointerArray { elem_alignment: 4 }),
        }),
        TypeRepr::Schema { schema } => {
            let sub = SchemaLayout::offsets_for(schema)?;
            Ok(FieldLayoutDecision {
                kind: FieldKind::PointerIndirect { tail_alignment: 4 },
                list_element: Some(ListElementKind::PointerArray {
                    elem_alignment: sub.root_align,
                }),
            })
        }
        // Nested `List<List<…>>`: each element is itself a `[len]
        // [payload]` list record addressed through a `u32` entry in the
        // pointer array, exactly like `List<String>` / `List<Schema>`.
        // The inner list record's own alignment depends on its element
        // (8 for Int/Float, 4 otherwise); the entry slots are 4-byte
        // offsets so the pointer array's `elem_alignment` is the inner
        // record's start alignment. Validating the inner element here
        // keeps the recursion bounded to shapes the writer/reader and
        // `relocate_pointers` actually materialise.
        TypeRepr::List { element: inner } => {
            let inner_align = inner_list_record_alignment(field_name, inner)?;
            Ok(FieldLayoutDecision {
                kind: FieldKind::PointerIndirect { tail_alignment: 4 },
                list_element: Some(ListElementKind::PointerArray {
                    elem_alignment: inner_align,
                }),
            })
        }
        TypeRepr::Option { .. } => Err(LayoutError::UnsupportedListElement {
            field: field_name.to_string(),
            inner: "Option",
        }),
        TypeRepr::Result { .. } => Err(LayoutError::UnsupportedListElement {
            field: field_name.to_string(),
            inner: "Result",
        }),
        TypeRepr::Null => Err(LayoutError::UnsupportedListElement {
            field: field_name.to_string(),
            inner: "Null",
        }),
        // Phase F.2: `List<Closure>` is not part of any v1 binary
        // surface — closures are non-portable scratch-heap pointers.
        TypeRepr::Closure { .. } => Err(LayoutError::UnsupportedListElement {
            field: field_name.to_string(),
            inner: "Closure",
        }),
    }
}

/// Start alignment of the `[len: u32][payload]` record an inner
/// `List<inner>` element occupies. Mirrors the `tail_alignment` each
/// scalar/String/Schema list field carries: `List<Int>` / `List<Float>`
/// records start 8-aligned so the i64/f64 payload lands aligned, every
/// other list record (Bool / String / Schema / a further nested List)
/// starts 4-aligned. Rejects element shapes the writer/reader don't
/// materialise so the nesting can't silently produce an undecodable
/// buffer.
fn inner_list_record_alignment(field_name: &str, inner: &TypeRepr) -> Result<usize, LayoutError> {
    match inner {
        // Inner *inline-fixed* element lists (`List<List<Int>>` /
        // `List<List<Float>>` / `List<List<Bool>>`): each inner record
        // is a self-contained `[len][payload]` with no internal pointer
        // slots, so the outer pointer array's per-entry rebase is the
        // only relocation needed. Int/Float records start 8-aligned for
        // the aligned payload; Bool records start 4-aligned.
        TypeRepr::Int | TypeRepr::Float => Ok(8),
        TypeRepr::Bool => Ok(4),
        // Inner *pointer-array* element lists (`List<List<String>>`,
        // `List<List<Schema>>`) and deeper nesting carry their own
        // internal pointer slots; rebasing them needs a recursive
        // pointer-array relocation the v1 reloc walker does not model.
        // Stay a loud cap rather than emit a buffer whose inner offsets
        // are off by `paste_base`.
        TypeRepr::String | TypeRepr::Schema { .. } | TypeRepr::List { .. } => {
            Err(LayoutError::UnsupportedListElement {
                field: field_name.to_string(),
                inner: "List<pointer-array element>",
            })
        }
        other => Err(LayoutError::UnsupportedListElement {
            field: field_name.to_string(),
            inner: match other {
                TypeRepr::Option { .. } => "Option",
                TypeRepr::Result { .. } => "Result",
                TypeRepr::Null => "Null",
                TypeRepr::Closure { .. } => "Closure",
                _ => "List",
            },
        }),
    }
}

/// Round `value` up to the next multiple of `align`. `align` is
/// assumed to be a non-zero power of two (the layout spec guarantees
/// alignments of 1 / 4 / 8 for v1; std's `checked_next_multiple_of`
/// works for any non-zero divisor and returns `None` on overflow).
fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align != 0, "alignment must be non-zero");
    value.checked_next_multiple_of(align)
}

/// Schema-level layout entry point. Computes the offset table for the
/// flat record area; tail-area layout (for String / List in Phase 2.b)
/// will be returned through a sibling helper once those types land.
pub struct SchemaLayout;

impl SchemaLayout {
    /// Compute the [`OffsetTable`] for `schema` under the v1 layout.
    ///
    /// The table covers only the fixed (root) area. `PointerIndirect`
    /// fields contribute a `u32` slot here; the actual tail-area
    /// payload is placed at write time by
    /// [`crate::buffer::BufferBuilder`].
    pub fn offsets_for(schema: &Schema) -> Result<OffsetTable, LayoutError> {
        let mut fields: Vec<FieldOffset> = Vec::with_capacity(schema.fields.len());
        let mut cursor: usize = 0;
        let mut max_align: usize = 1;

        for field in &schema.fields {
            let decision = field_layout_decision_for(&field.name, &field.ty)?;
            let kind = decision.kind;

            let (size, align) = match kind {
                FieldKind::Inline { size, align } => (size, align),
                // Pointer slot is always 4 bytes, 4-byte aligned, so
                // the wasm side can issue a single `i32.load offset=N`
                // regardless of the tail payload's alignment.
                FieldKind::PointerIndirect { .. } => (4, 4),
            };

            let offset = align_up(cursor, align).ok_or_else(|| LayoutError::Overflow {
                field: field.name.clone(),
                offset: cursor,
            })?;
            let next = offset
                .checked_add(size)
                .ok_or_else(|| LayoutError::Overflow {
                    field: field.name.clone(),
                    offset,
                })?;
            cursor = next;
            if align > max_align {
                max_align = align;
            }
            fields.push(FieldOffset {
                name: field.name.clone(),
                offset,
                size,
                align,
                kind,
                list_element: decision.list_element,
            });
        }

        // Pad the record to root_align so the next record after this
        // one (in a future struct-of-structs layout, or when the host
        // packs two #main calls back-to-back) starts aligned.
        let root_size = if schema.fields.is_empty() {
            0
        } else {
            align_up(cursor, max_align).ok_or_else(|| LayoutError::Overflow {
                field: "<root padding>".to_string(),
                offset: cursor,
            })?
        };

        Ok(OffsetTable {
            fields,
            root_size,
            root_align: max_align,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_canonical::{Field, Schema};

    fn field(name: &str, ty: TypeRepr) -> Field {
        Field {
            name: name.into(),
            ty,
            default: None,
        }
    }

    #[test]
    fn pure_int_fields_pack_at_eight_byte_stride() {
        let schema = Schema {
            name: "Trio".into(),
            generics: vec![],
            fields: vec![
                field("a", TypeRepr::Int),
                field("b", TypeRepr::Int),
                field("c", TypeRepr::Int),
            ],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[1].offset, 8);
        assert_eq!(table.fields[2].offset, 16);
        assert_eq!(table.root_size, 24);
        assert_eq!(table.root_align, 8);
    }

    #[test]
    fn int_then_bool_pads_after_int() {
        // Int at 0..8, Bool at 8..9, then pad up to 16 to honour root
        // alignment of 8 (max field align).
        let schema = Schema {
            name: "Pair".into(),
            generics: vec![],
            fields: vec![field("score", TypeRepr::Int), field("flag", TypeRepr::Bool)],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[0].size, 8);
        assert_eq!(table.fields[1].offset, 8);
        assert_eq!(table.fields[1].size, 1);
        assert_eq!(table.root_size, 16);
        assert_eq!(table.root_align, 8);
    }

    #[test]
    fn bool_then_int_pads_between_fields() {
        // Bool at 0..1, then 7 bytes of padding so the Int sits at 8.
        let schema = Schema {
            name: "Pair".into(),
            generics: vec![],
            fields: vec![field("flag", TypeRepr::Bool), field("score", TypeRepr::Int)],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[1].offset, 8);
        assert_eq!(table.root_size, 16);
        assert_eq!(table.root_align, 8);
    }

    #[test]
    fn pure_bool_fields_pack_tightly() {
        // Three bools, all 1-byte aligned: 0, 1, 2, root_size = 3,
        // root_align = 1 (no trailing padding needed).
        let schema = Schema {
            name: "Flags".into(),
            generics: vec![],
            fields: vec![
                field("a", TypeRepr::Bool),
                field("b", TypeRepr::Bool),
                field("c", TypeRepr::Bool),
            ],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[1].offset, 1);
        assert_eq!(table.fields[2].offset, 2);
        assert_eq!(table.root_size, 3);
        assert_eq!(table.root_align, 1);
    }

    #[test]
    fn empty_schema_has_zero_size_and_align_one() {
        let schema = Schema {
            name: "Unit".into(),
            generics: vec![],
            fields: vec![],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert!(table.fields.is_empty());
        assert_eq!(table.root_size, 0);
        assert_eq!(table.root_align, 1);
    }

    #[test]
    fn string_field_takes_one_pointer_slot() {
        // Phase 2.c: String contributes a 4-byte pointer slot in the
        // fixed area. The actual `[len][bytes]` payload lives in the
        // tail area appended by `BufferBuilder::write_string`.
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[0].size, 4);
        assert_eq!(table.fields[0].align, 4);
        assert!(matches!(
            table.fields[0].kind,
            FieldKind::PointerIndirect { tail_alignment: 1 }
        ));
        assert_eq!(table.root_size, 4);
        assert_eq!(table.root_align, 4);
        assert!(table.requires_tail_area());
        assert_eq!(table.fixed_area_size(), 4);
    }

    #[test]
    fn list_of_int_field_takes_one_pointer_slot() {
        // Same shape as String but the tail elements are 8-byte i64s,
        // so the kind records `tail_alignment: 8` for the builder.
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
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[0].size, 4);
        assert_eq!(table.fields[0].align, 4);
        assert!(matches!(
            table.fields[0].kind,
            FieldKind::PointerIndirect { tail_alignment: 8 }
        ));
    }

    #[test]
    fn list_of_string_takes_pointer_array_layout() {
        // Phase 10-c: List<String> is supported through a pointer
        // array. The fixed-area slot is the standard 4-byte pointer;
        // the element kind records `PointerArray { elem_alignment: 4 }`
        // so the builder pads each String len-prefix to 4 bytes.
        let schema = Schema {
            name: "Names".into(),
            generics: vec![],
            fields: vec![field(
                "names",
                TypeRepr::List {
                    element: Box::new(TypeRepr::String),
                },
            )],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[0].size, 4);
        assert_eq!(table.fields[0].align, 4);
        assert!(matches!(
            table.fields[0].kind,
            FieldKind::PointerIndirect { tail_alignment: 4 }
        ));
        assert!(matches!(
            table.fields[0].list_element,
            Some(ListElementKind::PointerArray { elem_alignment: 4 })
        ));
    }

    #[test]
    fn list_of_float_takes_inline_eight_byte_elements() {
        let schema = Schema {
            name: "Speeds".into(),
            generics: vec![],
            fields: vec![field(
                "xs",
                TypeRepr::List {
                    element: Box::new(TypeRepr::Float),
                },
            )],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert!(matches!(
            table.fields[0].kind,
            FieldKind::PointerIndirect { tail_alignment: 8 }
        ));
        assert!(matches!(
            table.fields[0].list_element,
            Some(ListElementKind::InlineFixed {
                elem_size: 8,
                elem_align: 8
            })
        ));
    }

    #[test]
    fn list_of_bool_packs_one_byte_per_element() {
        let schema = Schema {
            name: "Flags".into(),
            generics: vec![],
            fields: vec![field(
                "xs",
                TypeRepr::List {
                    element: Box::new(TypeRepr::Bool),
                },
            )],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert!(matches!(
            table.fields[0].kind,
            FieldKind::PointerIndirect { tail_alignment: 4 }
        ));
        assert!(matches!(
            table.fields[0].list_element,
            Some(ListElementKind::InlineFixed {
                elem_size: 1,
                elem_align: 1
            })
        ));
    }

    #[test]
    fn list_of_nested_scalar_list_is_pointer_array() {
        // `List<List<Int>>`: each element is an inner `[len][pad][i64]`
        // record (8-aligned start) addressed through the outer pointer
        // array, exactly like `List<String>` / `List<Schema>`.
        let schema = Schema {
            name: "Nested".into(),
            generics: vec![],
            fields: vec![field(
                "xs",
                TypeRepr::List {
                    element: Box::new(TypeRepr::List {
                        element: Box::new(TypeRepr::Int),
                    }),
                },
            )],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("nested scalar list accepted");
        assert_eq!(
            table.fields[0].list_element,
            Some(ListElementKind::PointerArray { elem_alignment: 8 })
        );
    }

    #[test]
    fn list_of_inner_pointer_array_list_is_rejected() {
        // `List<List<String>>`: the inner list record is itself a
        // pointer array carrying internal slots the v1 reloc walker
        // can't rebase — stays a loud cap via UnsupportedListElement.
        let schema = Schema {
            name: "Nested".into(),
            generics: vec![],
            fields: vec![field(
                "xs",
                TypeRepr::List {
                    element: Box::new(TypeRepr::List {
                        element: Box::new(TypeRepr::String),
                    }),
                },
            )],
        };
        let err = SchemaLayout::offsets_for(&schema).expect_err("must reject");
        assert!(matches!(
            err,
            LayoutError::UnsupportedListElement {
                inner: "List<pointer-array element>",
                ..
            }
        ));
    }

    #[test]
    fn list_of_schema_picks_subrecord_alignment() {
        // Sub-schema with an Int field demands 8-byte alignment for
        // its fixed area, so the pointer-array elements need 8-byte
        // alignment when the builder lays them out.
        let inner = Schema {
            name: "Inner".into(),
            generics: vec![],
            fields: vec![field("v", TypeRepr::Int)],
        };
        let schema = Schema {
            name: "Outer".into(),
            generics: vec![],
            fields: vec![field(
                "xs",
                TypeRepr::List {
                    element: Box::new(TypeRepr::Schema {
                        schema: Box::new(inner),
                    }),
                },
            )],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert!(matches!(
            table.fields[0].list_element,
            Some(ListElementKind::PointerArray { elem_alignment: 8 })
        ));
    }

    #[test]
    fn string_then_int_packs_pointer_then_padding_then_int() {
        // Pointer slot at 0..4, padding 4..8, Int at 8..16. Root
        // alignment is 8 because the Int slot demands it.
        let schema = Schema {
            name: "Mixed".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String), field("age", TypeRepr::Int)],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[0].size, 4);
        assert_eq!(table.fields[1].offset, 8);
        assert_eq!(table.fields[1].size, 8);
        assert_eq!(table.root_size, 16);
        assert_eq!(table.root_align, 8);
        assert!(table.requires_tail_area());
    }

    #[test]
    fn null_field_is_one_byte_one_aligned() {
        let schema = Schema {
            name: "Sentinel".into(),
            generics: vec![],
            fields: vec![field("nil", TypeRepr::Null)],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[0].offset, 0);
        assert_eq!(table.fields[0].size, 1);
        assert_eq!(table.fields[0].align, 1);
        assert_eq!(table.root_size, 1);
        assert_eq!(table.root_align, 1);
    }

    #[test]
    fn float_field_is_eight_byte_aligned() {
        let schema = Schema {
            name: "Phys".into(),
            generics: vec![],
            fields: vec![
                field("flag", TypeRepr::Bool),
                field("mass", TypeRepr::Float),
            ],
        };
        let table = SchemaLayout::offsets_for(&schema).expect("layout");
        assert_eq!(table.fields[1].offset, 8);
        assert_eq!(table.fields[1].size, 8);
        assert_eq!(table.fields[1].align, 8);
        assert_eq!(table.root_size, 16);
    }
}
