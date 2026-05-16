//! Static field-offset table for the host <-> wasm binary handshake.
//!
//! Spec: `docs/internal/wasm-binary-layout-v1-2026-05-16.md`,
//! "basic type layout" + "slot alignment rule" sections.
//!
//! v1 scope (Phase 2.a):
//!
//! * Only the four scalar leaves — `Int`, `Float`, `Bool`, `Null` —
//!   are laid out. Variable-size leaves (`String`, `List`, `Dict`,
//!   `Option`, `Result`, `Enum`) need pointer-indirection tail areas
//!   that aren't modelled here yet; the layout pass returns
//!   [`LayoutError::UnsupportedTypeInLayoutV1`] for those so callers
//!   fail loudly rather than silently producing the wrong offsets.
//! * Field order matches declaration order; each field's start offset
//!   is rounded up to the field's alignment. Root size is padded up
//!   to the max alignment so the next record (in a future struct-of-
//!   structs layout) starts aligned too.
//!
//! Phase 2.b extends this module to handle `String` / `List` via the
//! pointer-indirected tail area described in the layout spec. The
//! offset table for the fixed record part stays compatible — variable
//! fields contribute a `u32` slot in the fixed area, and the tail
//! area is appended after `root_size`.

use crate::schema_canonical::{Schema, TypeRepr};
use thiserror::Error;

/// One field's slot inside a record.
///
/// `offset` is the byte position relative to the buffer base. `size`
/// is the slot's contribution to the fixed record area (including
/// any internal padding for non-scalar types in later phases). `align`
/// is the slot's required alignment in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldOffset {
    /// Field name as declared in the source schema.
    pub name: String,
    /// Byte offset from the record base. Always a multiple of `align`.
    pub offset: usize,
    /// Slot size in bytes (excluding trailing padding to root_align).
    pub size: usize,
    /// Required alignment of this slot's start in bytes.
    pub align: usize,
}

/// Computed offset table for a schema's flat record area.
///
/// `root_size` is the total record size after padding the last field
/// up to `root_align`. `root_align` is the maximum of every field's
/// alignment (or `1` for an empty schema — `0` would be a malformed
/// alignment).
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

/// Computed `(size, align)` for one scalar leaf, or `None` for a type
/// the v1 layout doesn't yet support. Bool / Null are 1-byte
/// 1-aligned; Int / Float are 8-byte 8-aligned. The numbers match the
/// spec's "basic type layout" table verbatim.
fn scalar_size_align(ty: &TypeRepr) -> Option<(usize, usize)> {
    match ty {
        TypeRepr::Null => Some((1, 1)),
        TypeRepr::Bool => Some((1, 1)),
        TypeRepr::Int => Some((8, 8)),
        TypeRepr::Float => Some((8, 8)),
        // Variable / compound types deferred to Phase 2.b.
        TypeRepr::String
        | TypeRepr::List { .. }
        | TypeRepr::Option { .. }
        | TypeRepr::Result { .. }
        | TypeRepr::Schema { .. } => None,
    }
}

/// Human-readable name used in `UnsupportedTypeInLayoutV1` for one of
/// the v1-unsupported type kinds. Returns `None` for types v1 does
/// support (used at the call site to detect "we need a kind label").
fn unsupported_kind(ty: &TypeRepr) -> Option<&'static str> {
    match ty {
        TypeRepr::String => Some("String"),
        TypeRepr::List { .. } => Some("List"),
        TypeRepr::Option { .. } => Some("Option"),
        TypeRepr::Result { .. } => Some("Result"),
        TypeRepr::Schema { .. } => Some("Schema"),
        // Scalar leaves are supported — caller should not reach the
        // unsupported branch for these.
        TypeRepr::Null | TypeRepr::Bool | TypeRepr::Int | TypeRepr::Float => None,
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
    pub fn offsets_for(schema: &Schema) -> Result<OffsetTable, LayoutError> {
        let mut fields: Vec<FieldOffset> = Vec::with_capacity(schema.fields.len());
        let mut cursor: usize = 0;
        let mut max_align: usize = 1;

        for field in &schema.fields {
            let (size, align) = match scalar_size_align(&field.ty) {
                Some(pair) => pair,
                None => {
                    let kind = unsupported_kind(&field.ty)
                        .expect("scalar_size_align returned None only for unsupported variants");
                    return Err(LayoutError::UnsupportedTypeInLayoutV1 {
                        field: field.name.clone(),
                        kind,
                    });
                }
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
    fn string_field_is_rejected_for_v1() {
        // String layout requires pointer-indirection; the v1 pass
        // refuses to invent a placeholder so callers detect the gap.
        let schema = Schema {
            name: "Greet".into(),
            generics: vec![],
            fields: vec![field("name", TypeRepr::String)],
        };
        let err = SchemaLayout::offsets_for(&schema).expect_err("must reject");
        assert!(matches!(
            err,
            LayoutError::UnsupportedTypeInLayoutV1 { kind: "String", .. }
        ));
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
