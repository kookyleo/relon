//! `relon_analyzer::SchemaDef` -> [`crate::schema_canonical::Schema`]
//! conversion.
//!
//! The wasm AOT backend needs a deterministic, ABI-shaped view of a
//! schema so it can compute the same `relon.abi` hash both at codegen
//! time (host side) and at validation time (wasm-blob loader). The
//! analyzer's [`SchemaDef`] is the static skeleton the rest of the
//! pipeline reasons about; this module strips it down to the field
//! `(name, type)` pairs the binary layout cares about.
//!
//! Phase 2.b scope: only `Int` / `Float` / `Bool` / `Null` field types
//! are supported. Everything else returns a [`SchemaLowerError`] so
//! callers fail loudly rather than silently producing the wrong hash.
//! String / List / Option / Result / Schema-nested fields land in
//! Phase 2.c when the layout pass grows the pointer-indirection tail
//! area.

use crate::schema_canonical::{Field, Schema, TypeRepr};
use relon_analyzer::schema::SchemaDef;
use relon_parser::TypeNode;
use thiserror::Error;

/// Reasons schema lowering can fail.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SchemaLowerError {
    /// A field used a type Phase 2.b's layout pass does not yet
    /// model. The string in `ty` is the type head as written in
    /// source (e.g. `"String"`, `"List<Int>"`), so the user sees
    /// the exact annotation that triggered the gap.
    #[error("field `{field}` has unsupported type `{ty}` (Phase 2.b layout supports Int / Float / Bool / Null only)")]
    UnsupportedFieldType {
        /// Field name that triggered the error.
        field: String,
        /// Human-readable rendering of the offending type.
        ty: String,
    },
    /// A field declared no static type. The analyzer surfaces this as
    /// `SchemaFieldUntyped` in its own diagnostics; we propagate the
    /// shape so codegen can refuse to emit a layout with unknown
    /// slot widths.
    #[error("field `{field}` has no declared type")]
    UntypedField {
        /// Field name that triggered the error.
        field: String,
    },
}

/// Lower a [`SchemaDef`] to its canonical [`Schema`] form.
///
/// The output preserves declaration order (canonical schemas hash
/// order-sensitively) and uses the supplied `name` as the canonical
/// schema name when the analyzer-side `SchemaDef::name` is `None`
/// (anonymous `#schema` annotations on data).
pub fn lower_schema_def(def: &SchemaDef, fallback_name: &str) -> Result<Schema, SchemaLowerError> {
    let mut fields = Vec::with_capacity(def.fields.len());
    for f in &def.fields {
        let ty_node = f
            .type_hint
            .as_ref()
            .ok_or_else(|| SchemaLowerError::UntypedField {
                field: f.name.clone(),
            })?;
        let ty = lower_type_node(&f.name, ty_node)?;
        fields.push(Field {
            name: f.name.clone(),
            ty,
            // Phase 2.b ignores compile-time defaults — the layout
            // pass only needs the slot shape. Defaults re-enter the
            // canonical form when the codegen pipeline starts
            // populating them in a later phase.
            default: None,
        });
    }
    Ok(Schema {
        name: def
            .name
            .clone()
            .unwrap_or_else(|| fallback_name.to_string()),
        generics: def.generics.clone(),
        fields,
    })
}

/// Lower a single [`TypeNode`] to a [`TypeRepr`]. Rejects every
/// composite / variable-size type — see [`SchemaLowerError`].
pub fn lower_type_node(field_name: &str, ty: &TypeNode) -> Result<TypeRepr, SchemaLowerError> {
    let unsupported = || SchemaLowerError::UnsupportedFieldType {
        field: field_name.to_string(),
        ty: format_type_head(ty),
    };
    if ty.path.len() != 1 || !ty.generics.is_empty() || ty.variant_fields.is_some() {
        return Err(unsupported());
    }
    match ty.path[0].as_str() {
        "Int" => Ok(TypeRepr::Int),
        "Float" => Ok(TypeRepr::Float),
        "Bool" => Ok(TypeRepr::Bool),
        "Null" => Ok(TypeRepr::Null),
        _ => Err(unsupported()),
    }
}

/// Format a `TypeNode` head + generics for the error message. Local
/// to this module so we don't drag the analyzer's full type
/// formatter through the dependency graph.
fn format_type_head(t: &TypeNode) -> String {
    if t.path.is_empty() {
        return "<empty>".to_string();
    }
    let mut s = t.path.join(".");
    if !t.generics.is_empty() {
        s.push('<');
        for (i, g) in t.generics.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&format_type_head(g));
        }
        s.push('>');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_analyzer::schema::SchemaFieldDef;
    use relon_parser::{Expr, Node, NodeId, TokenRange, TypeNode};
    use std::sync::Arc;

    fn dummy_range() -> TokenRange {
        TokenRange::default()
    }

    fn dummy_node() -> Arc<Node> {
        Arc::new(Node {
            id: NodeId::SYNTHETIC,
            expr: Box::new(Expr::Int(0)),
            decorators: vec![],
            directives: vec![],
            type_hint: None,
            range: dummy_range(),
            doc_comment: None,
        })
    }

    fn type_node(name: &str) -> TypeNode {
        TypeNode {
            path: vec![name.to_string()],
            generics: vec![],
            is_optional: false,
            range: dummy_range(),
            variant_fields: None,
            doc_comment: None,
        }
    }

    fn field(name: &str, ty: TypeNode) -> SchemaFieldDef {
        SchemaFieldDef {
            name: name.to_string(),
            type_hint: Some(ty),
            value_range: dummy_range(),
            is_wildcard: true,
            value_node: dummy_node(),
            meta_decorators: vec![],
            doc_comment: None,
        }
    }

    fn schema_def(name: &str, fields: Vec<SchemaFieldDef>) -> SchemaDef {
        SchemaDef {
            name: Some(name.to_string()),
            generics: vec![],
            fields,
            bases: vec![],
            range: dummy_range(),
            variants: vec![],
            methods: vec![],
            schema_no_auto_derives: vec![],
            doc_comment: None,
        }
    }

    #[test]
    fn lowers_int_float_bool_null() {
        let def = schema_def(
            "Mix",
            vec![
                field("a", type_node("Int")),
                field("b", type_node("Float")),
                field("c", type_node("Bool")),
                field("d", type_node("Null")),
            ],
        );
        let s = lower_schema_def(&def, "fallback").expect("lower");
        assert_eq!(s.name, "Mix");
        assert_eq!(s.fields.len(), 4);
        assert_eq!(s.fields[0].ty, TypeRepr::Int);
        assert_eq!(s.fields[1].ty, TypeRepr::Float);
        assert_eq!(s.fields[2].ty, TypeRepr::Bool);
        assert_eq!(s.fields[3].ty, TypeRepr::Null);
    }

    #[test]
    fn rejects_string_field() {
        let def = schema_def("S", vec![field("name", type_node("String"))]);
        let err = lower_schema_def(&def, "fallback").expect_err("must reject");
        assert!(matches!(
            err,
            SchemaLowerError::UnsupportedFieldType { ref field, ref ty }
            if field == "name" && ty == "String"
        ));
    }

    #[test]
    fn rejects_untyped_field() {
        let mut def = schema_def("S", vec![field("x", type_node("Int"))]);
        def.fields[0].type_hint = None;
        let err = lower_schema_def(&def, "fallback").expect_err("must reject");
        assert!(matches!(
            err,
            SchemaLowerError::UntypedField { ref field } if field == "x"
        ));
    }

    #[test]
    fn anonymous_schema_uses_fallback_name() {
        let mut def = schema_def("ignored", vec![field("v", type_node("Int"))]);
        def.name = None;
        let s = lower_schema_def(&def, "MainParams").expect("lower");
        assert_eq!(s.name, "MainParams");
    }
}
