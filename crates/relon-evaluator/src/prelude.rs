//! Built-in schemas seeded into every [`crate::Context`].
//!
//! Currently `Result<T, E>` and `Option<T>`, both expressed as
//! tagged-enum schemas (`Value::EnumSchema`) with a single value-payload
//! field per non-unit variant. Seeding them in [`Context::new`] means
//! user code can write
//!
//! ```relon
//! #main(...) -> Result<Order, String>
//! { result: Result.Ok { value: order } }
//! ```
//!
//! without any explicit `#schema Result<...>` declaration. Users who
//! want to override (e.g. a custom `Option`) can still call
//! [`crate::Context::register_schema`] — that simply replaces the
//! prelude entry in the per-context schema table.

use crate::value::{SchemaField, Value};
use relon_parser::{TokenRange, TypeNode};
use std::collections::HashMap;

/// Inject `Result` and `Option` into a fresh schema table.
pub(crate) fn seed_prelude_schemas(schemas: &mut HashMap<String, Value>) {
    schemas.insert("Result".to_string(), build_result());
    schemas.insert("Option".to_string(), build_option());
}

/// A bare type-variable reference (no path segments, no nested
/// generics), used inside the synthetic `SchemaField::type_hint`s of
/// each prelude variant.
fn type_var(name: &str) -> TypeNode {
    TypeNode {
        path: vec![name.to_string()],
        generics: Vec::new(),
        is_optional: false,
        range: TokenRange::default(),
        variant_fields: None,
        doc_comment: None,
    }
}

/// A wildcard-predicate schema field carrying just the given type.
/// Variants in the prelude only need a single `value`/`error` field.
fn wildcard_field(t: TypeNode) -> SchemaField {
    SchemaField {
        type_hint: t,
        predicates: vec![Value::Wildcard],
        custom_error: None,
        default_value: None,
    }
}

fn build_result() -> Value {
    let mut variants = HashMap::new();

    let mut ok_fields = HashMap::new();
    ok_fields.insert("value".to_string(), wildcard_field(type_var("T")));
    variants.insert("Ok".to_string(), ok_fields);

    let mut err_fields = HashMap::new();
    err_fields.insert("error".to_string(), wildcard_field(type_var("E")));
    variants.insert("Err".to_string(), err_fields);

    Value::EnumSchema(Box::new(crate::value::EnumSchemaData {
        name: "Result".to_string(),
        generics: vec!["T".to_string(), "E".to_string()],
        variants,
    }))
}

fn build_option() -> Value {
    let mut variants = HashMap::new();

    let mut some_fields = HashMap::new();
    some_fields.insert("value".to_string(), wildcard_field(type_var("T")));
    variants.insert("Some".to_string(), some_fields);

    // Unit variant: no fields.
    variants.insert("None".to_string(), HashMap::new());

    Value::EnumSchema(Box::new(crate::value::EnumSchemaData {
        name: "Option".to_string(),
        generics: vec!["T".to_string()],
        variants,
    }))
}
