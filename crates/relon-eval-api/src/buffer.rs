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

use crate::layout::OffsetTable;
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
}

/// Type-checked writer over a fixed-size record buffer.
///
/// Lifetime tie-in: the builder borrows the offset table so the same
/// layout description can be reused for the matching reader without
/// reparsing. The internal `bytes: Vec<u8>` is initialised to
/// `vec![0u8; layout.root_size]` so every field has well-defined
/// padding bytes (`0x00`) per the spec.
#[derive(Debug)]
pub struct BufferBuilder<'a> {
    layout: &'a OffsetTable,
    field_index: Vec<(String, TypeRepr, usize, usize)>,
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
                    .map(|f| (fo.name.clone(), f.ty.clone(), fo.offset, fo.size))
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
        let (offset, _) = self.locate(field_name, &TypeRepr::Int, "Int")?;
        self.bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Write a 64-bit float to `field_name`.
    pub fn write_float(&mut self, field_name: &str, value: f64) -> Result<(), BufferError> {
        let (offset, _) = self.locate(field_name, &TypeRepr::Float, "Float")?;
        self.bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Write a boolean to `field_name`. Encoded as `0u8` / `1u8`.
    pub fn write_bool(&mut self, field_name: &str, value: bool) -> Result<(), BufferError> {
        let (offset, _) = self.locate(field_name, &TypeRepr::Bool, "Bool")?;
        self.bytes[offset] = u8::from(value);
        Ok(())
    }

    /// Mark `field_name` as `Null`. The slot is already zeroed by
    /// `new`, so this is a no-op beyond the type check — useful to
    /// surface a `TypeMismatch` early when the host accidentally
    /// nulls a non-Null slot.
    pub fn write_null(&mut self, field_name: &str) -> Result<(), BufferError> {
        let (_, _) = self.locate(field_name, &TypeRepr::Null, "Null")?;
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
    ) -> Result<(usize, usize), BufferError> {
        let entry = self
            .field_index
            .iter()
            .find(|(name, _, _, _)| name == field_name)
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
        Ok((entry.2, entry.3))
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

/// Type-checked reader over a fixed-size record buffer.
///
/// The buffer is borrowed (no copy), so reads cost a bounds check
/// plus a `from_le_bytes`. As with [`BufferBuilder`], the reader
/// carries a side index of `(name, type, offset, size)` so type
/// mismatches surface at the call site.
#[derive(Debug)]
pub struct BufferReader<'a> {
    layout: &'a OffsetTable,
    field_index: Vec<(String, TypeRepr, usize, usize)>,
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
                    .map(|f| (fo.name.clone(), f.ty.clone(), fo.offset, fo.size))
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
        let (offset, _) = self.locate(field_name, &TypeRepr::Int, "Int")?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.bytes[offset..offset + 8]);
        Ok(i64::from_le_bytes(buf))
    }

    /// Read a 64-bit float from `field_name`.
    pub fn read_float(&self, field_name: &str) -> Result<f64, BufferError> {
        let (offset, _) = self.locate(field_name, &TypeRepr::Float, "Float")?;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.bytes[offset..offset + 8]);
        Ok(f64::from_le_bytes(buf))
    }

    /// Read a boolean from `field_name`. Any non-zero byte decodes
    /// as `true` (the layout only writes 0 or 1, but defensive
    /// decoding makes the reader robust against buffer corruption).
    pub fn read_bool(&self, field_name: &str) -> Result<bool, BufferError> {
        let (offset, _) = self.locate(field_name, &TypeRepr::Bool, "Bool")?;
        Ok(self.bytes[offset] != 0)
    }

    /// Confirm `field_name` is declared as `Null` and that the slot
    /// is reachable. The byte value is unused (Null slots are
    /// tag-only), so this only validates the type label.
    pub fn read_null(&self, field_name: &str) -> Result<(), BufferError> {
        let (_, _) = self.locate(field_name, &TypeRepr::Null, "Null")?;
        Ok(())
    }

    fn locate(
        &self,
        field_name: &str,
        expected: &TypeRepr,
        requested_label: &'static str,
    ) -> Result<(usize, usize), BufferError> {
        let entry = self
            .field_index
            .iter()
            .find(|(name, _, _, _)| name == field_name)
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
        Ok((entry.2, entry.3))
    }

    /// Read-only access to the layout this reader walks.
    #[allow(dead_code)]
    pub(crate) fn layout(&self) -> &OffsetTable {
        self.layout
    }
}

/// Compare a schema-declared [`TypeRepr`] against the one a writer /
/// reader assumed. v1 only ever calls this on scalar leaves, so the
/// match is exact (no subtype rules).
fn type_matches(declared: &TypeRepr, requested: &TypeRepr) -> bool {
    matches!(
        (declared, requested),
        (TypeRepr::Int, TypeRepr::Int)
            | (TypeRepr::Float, TypeRepr::Float)
            | (TypeRepr::Bool, TypeRepr::Bool)
            | (TypeRepr::Null, TypeRepr::Null)
    )
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
