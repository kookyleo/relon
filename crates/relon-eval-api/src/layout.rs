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

/// Compute the field-kind / fixed-area placement for one [`TypeRepr`].
///
/// Returns `None` only for types Phase 2.c still rejects (`Dict`,
/// `Option`, `Result`, nested schemas, lists of non-`Int` element
/// types). The supported set:
///
/// * `Null`, `Bool` → `Inline { size: 1, align: 1 }`.
/// * `Int`, `Float` → `Inline { size: 8, align: 8 }`.
/// * `String` → `PointerIndirect { tail_alignment: 1 }` (utf-8 bytes
///   need no alignment beyond the leading `[len: u32]`'s implicit
///   4-byte boundary).
/// * `List<Int>` → `PointerIndirect { tail_alignment: 8 }` (i64
///   elements are emitted inline in the tail area).
fn field_kind_for(ty: &TypeRepr) -> Option<FieldKind> {
    match ty {
        TypeRepr::Null => Some(FieldKind::Inline { size: 1, align: 1 }),
        TypeRepr::Bool => Some(FieldKind::Inline { size: 1, align: 1 }),
        TypeRepr::Int => Some(FieldKind::Inline { size: 8, align: 8 }),
        TypeRepr::Float => Some(FieldKind::Inline { size: 8, align: 8 }),
        TypeRepr::String => Some(FieldKind::PointerIndirect { tail_alignment: 1 }),
        TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int) => {
            Some(FieldKind::PointerIndirect { tail_alignment: 8 })
        }
        // Phase 2.c still defers nested-schema / option / result /
        // dict and any `List<T>` where `T != Int` to a later phase.
        TypeRepr::List { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Schema { .. } => None,
    }
}

/// Human-readable name used in `UnsupportedTypeInLayoutV1`. Returns
/// `None` for types Phase 2.c does support inline or via pointer
/// indirection. Used at the call site to detect "we need a kind
/// label".
fn unsupported_kind(ty: &TypeRepr) -> Option<&'static str> {
    match ty {
        TypeRepr::Option { .. } => Some("Option"),
        TypeRepr::Result { .. } => Some("Result"),
        TypeRepr::Schema { .. } => Some("Schema"),
        // `List<T>` where `T != Int` lands here; the caller has
        // already established `field_kind_for` returned `None` for
        // this exact shape, so emitting "List" is correct.
        TypeRepr::List { .. } => Some("List"),
        // Scalar leaves + supported pointer-indirect leaves are
        // supported — caller should not reach the unsupported branch
        // for these.
        TypeRepr::Null | TypeRepr::Bool | TypeRepr::Int | TypeRepr::Float | TypeRepr::String => {
            None
        }
    }
}

/// Round `value` up to the next multiple of `align`. `align` is
/// assumed to be a non-zero power of two (the layout spec guarantees
/// alignments of 1 / 4 / 8 for v1; the routine still works for any
/// non-zero divisor).
fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align != 0, "alignment must be non-zero");
    // Equivalent to `(value + align - 1) & !(align - 1)` for power-
    // of-two alignments, but we use the modulo form so the routine
    // doesn't silently produce garbage for a hypothetical non-power-
    // of-two alignment a future type might want.
    let rem = value % align;
    if rem == 0 {
        return Some(value);
    }
    value.checked_add(align - rem)
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
            let kind = match field_kind_for(&field.ty) {
                Some(k) => k,
                None => {
                    let kind = unsupported_kind(&field.ty)
                        .expect("field_kind_for returned None only for unsupported variants");
                    return Err(LayoutError::UnsupportedTypeInLayoutV1 {
                        field: field.name.clone(),
                        kind,
                    });
                }
            };

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
    fn list_of_string_is_rejected_in_phase_2c() {
        // Lists are gated on element type — Phase 2.c only opens up
        // `List<Int>`. `List<String>` waits for Phase 3.
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
        let err = SchemaLayout::offsets_for(&schema).expect_err("must reject");
        assert!(matches!(
            err,
            LayoutError::UnsupportedTypeInLayoutV1 { kind: "List", .. }
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
