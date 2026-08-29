//! Type-directed JSON decoding for host-supplied Relon values.
//!
//! JSON itself cannot distinguish a list from a tuple, infer the payload
//! type of an enum variant, or decide whether `null` means `Option.None`.
//! This module decodes against the analyzer's declared [`TypeNode`]s so
//! every host boundary (CLI, WASM, and embedders) follows one policy.

use crate::Value;
use relon_analyzer::{
    format_type, substitute_generics_in_typenode, AnalyzedTree, EnumVariant, MainSignature,
    SchemaDef, SchemaFieldDef,
};
use relon_parser::TypeNode;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Failure to decode a JSON value against a Relon type declaration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct JsonDecodeError {
    message: String,
}

impl JsonDecodeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Human-readable explanation without an additional error prefix.
    pub fn message(&self) -> &str {
        &self.message
    }
}

type DecodeResult<T> = std::result::Result<T, JsonDecodeError>;

fn fail<T>(message: impl Into<String>) -> DecodeResult<T> {
    Err(JsonDecodeError::new(message))
}

/// Decode a JSON object against a program's `#main(...)` signature.
///
/// Known parameter names are decoded using their declared types. Unknown
/// names are decoded without a type hint and are left for `run_main` to
/// reject as unexpected arguments; this preserves the evaluator's canonical
/// argument-validation error.
pub fn decode_main_args(
    json: JsonValue,
    signature: &MainSignature,
    tree: &AnalyzedTree,
) -> std::result::Result<HashMap<String, Value>, JsonDecodeError> {
    let JsonValue::Object(raw_args) = json else {
        return fail(format!(
            "args must be a JSON object keyed by `#main(...)` parameter names; got {}",
            json_kind(&json)
        ));
    };
    let param_types: HashMap<&str, &TypeNode> = signature
        .params
        .iter()
        .map(|param| (param.name.as_str(), &param.type_node))
        .collect();
    let mut args = HashMap::with_capacity(raw_args.len());
    for (name, json) in raw_args {
        let value = if let Some(type_hint) = param_types.get(name.as_str()) {
            decode_json_for_type(json, type_hint, tree).map_err(|error| {
                JsonDecodeError::new(format!(
                    "args value for `{name}` cannot be decoded as {}: {error}",
                    format_type(type_hint)
                ))
            })?
        } else {
            targetless_json_to_value(json)?
        };
        args.insert(name, value);
    }
    Ok(args)
}

/// Decode one JSON value against a Relon type declaration.
///
/// This is the lower-level counterpart of [`decode_main_args`] for hosts
/// that already selected a target [`TypeNode`]. Schema and enum lookups use
/// `tree`, including imported schema metadata and generic substitution.
pub fn decode_json_for_type(
    json: JsonValue,
    type_hint: &TypeNode,
    tree: &AnalyzedTree,
) -> std::result::Result<Value, JsonDecodeError> {
    if type_hint.is_optional {
        let mut inner_type = type_hint.clone();
        inner_type.is_optional = false;
        return decode_option(json, Some(&inner_type), tree);
    }

    match type_key(type_hint).as_str() {
        "Option" => return decode_option(json, type_hint.generics.first(), tree),
        "Result" => {
            return decode_result(
                json,
                type_hint.generics.first(),
                type_hint.generics.get(1),
                tree,
            );
        }
        _ => {}
    }

    if let Some(value) = decode_builtin(json.clone(), type_hint) {
        return value;
    }

    match type_key(type_hint).as_str() {
        "Tuple" => decode_tuple(json, &type_hint.generics, tree),
        "List" => decode_list(json, type_hint.generics.first(), tree),
        "Dict" => decode_dict(json, type_hint.generics.get(1), tree),
        "Enum" => decode_enum_union(json, type_hint, tree),
        _ => {
            if let Some(def) = schema_def_for_type(type_hint, tree) {
                if !def.variants.is_empty() {
                    return decode_tagged_enum(json, type_hint, def, tree);
                }
            }
            if let Some(elements) = schema_tuple_elements(type_hint, tree) {
                decode_tuple(json, &elements, tree)
            } else if let Some(fields) = schema_fields(type_hint, tree) {
                decode_schema(json, &fields, tree)
            } else {
                targetless_json_to_value(json)
            }
        }
    }
}

fn decode_builtin(json: JsonValue, type_hint: &TypeNode) -> Option<DecodeResult<Value>> {
    let result = match type_key(type_hint).as_str() {
        "Any" => targetless_json_to_value(json),
        "Bool" => match json {
            JsonValue::Bool(value) => Ok(Value::Bool(value)),
            other => fail(format!("expected JSON bool, got {}", json_kind(&other))),
        },
        "Int" => match json {
            JsonValue::Number(value) => value
                .as_i64()
                .map(Value::Int)
                .ok_or_else(|| JsonDecodeError::new("expected JSON integer in i64 range")),
            other => fail(format!("expected JSON integer, got {}", json_kind(&other))),
        },
        "Float" => match json {
            JsonValue::Number(value) => value
                .as_f64()
                .map(|value| Value::Float(value.into()))
                .ok_or_else(|| JsonDecodeError::new("expected finite JSON number")),
            other => fail(format!("expected JSON number, got {}", json_kind(&other))),
        },
        "Number" => match json {
            JsonValue::Number(value) => {
                if let Some(value) = value.as_i64() {
                    Ok(Value::Int(value))
                } else {
                    value
                        .as_f64()
                        .map(|value| Value::Float(value.into()))
                        .ok_or_else(|| JsonDecodeError::new("expected finite JSON number"))
                }
            }
            other => fail(format!("expected JSON number, got {}", json_kind(&other))),
        },
        "String" => match json {
            JsonValue::String(value) => Ok(Value::String(value.into())),
            other => fail(format!("expected JSON string, got {}", json_kind(&other))),
        },
        _ => return None,
    };
    Some(result)
}

fn decode_option(
    json: JsonValue,
    inner_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    match json {
        JsonValue::Null => Ok(Value::option_none()),
        JsonValue::String(name) if name == "None" => Ok(Value::option_none()),
        JsonValue::Object(mut map) if map.len() == 1 && map.contains_key("None") => {
            match map.remove("None").expect("checked None payload") {
                JsonValue::Null => Ok(Value::option_none()),
                JsonValue::Object(map) if map.is_empty() => Ok(Value::option_none()),
                _ => fail("Option.None expects null or an empty object payload"),
            }
        }
        JsonValue::Object(mut map) if map.len() == 1 && map.contains_key("Some") => {
            let payload = map.remove("Some").expect("checked Some payload");
            decode_single_payload(payload, "value", inner_type, tree).map(Value::option_some)
        }
        other => decode_single_payload(other, "value", inner_type, tree).map(Value::option_some),
    }
}

fn decode_result(
    json: JsonValue,
    ok_type: Option<&TypeNode>,
    err_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    let JsonValue::Object(mut map) = json else {
        return fail(
            r#"Result<T, E> expects an externally tagged object: {"Ok": ...} or {"Err": ...}"#,
        );
    };
    if map.len() != 1 {
        return fail("Result<T, E> expects exactly one variant key: Ok or Err");
    }
    if let Some(payload) = map.remove("Ok") {
        return decode_single_payload(payload, "value", ok_type, tree).map(Value::result_ok);
    }
    if let Some(payload) = map.remove("Err") {
        return decode_single_payload(payload, "error", err_type, tree).map(Value::result_err);
    }
    let name = map.keys().next().cloned().unwrap_or_default();
    fail(format!(
        "object key `{name}` is not a Result variant; expected Ok or Err"
    ))
}

fn decode_single_payload(
    payload: JsonValue,
    field_name: &str,
    field_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    let Some(field_type) = field_type else {
        return targetless_json_to_value(payload);
    };
    match decode_json_for_type(payload.clone(), field_type, tree) {
        Ok(value) => Ok(value),
        Err(direct_error) => match payload {
            JsonValue::Object(mut map) if map.len() == 1 && map.contains_key(field_name) => {
                decode_json_for_type(
                    map.remove(field_name).expect("checked payload field"),
                    field_type,
                    tree,
                )
            }
            _ => Err(direct_error),
        },
    }
}

fn decode_tuple(
    json: JsonValue,
    element_types: &[TypeNode],
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    match json {
        JsonValue::Array(items) => items
            .into_iter()
            .enumerate()
            .map(|(index, item)| match element_types.get(index) {
                Some(type_hint) => decode_json_for_type(item, type_hint, tree),
                None => targetless_json_to_value(item),
            })
            .collect::<DecodeResult<Vec<_>>>()
            .map(Value::tuple),
        other => targetless_json_to_value(other),
    }
}

fn decode_list(
    json: JsonValue,
    item_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    match json {
        JsonValue::Array(items) => items
            .into_iter()
            .map(|item| match item_type {
                Some(type_hint) => decode_json_for_type(item, type_hint, tree),
                None => targetless_json_to_value(item),
            })
            .collect::<DecodeResult<Vec<_>>>()
            .map(Value::list),
        other => targetless_json_to_value(other),
    }
}

fn decode_dict(
    json: JsonValue,
    value_type: Option<&TypeNode>,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    match json {
        JsonValue::Object(map) => map
            .into_iter()
            .map(|(key, item)| {
                let value = match value_type {
                    Some(type_hint) => decode_json_for_type(item, type_hint, tree),
                    None => targetless_json_to_value(item),
                }?;
                Ok((key, value))
            })
            .collect::<DecodeResult<Vec<_>>>()
            .map(Value::dict),
        other => targetless_json_to_value(other),
    }
}

fn decode_schema(
    json: JsonValue,
    field_types: &HashMap<String, TypeNode>,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    match json {
        JsonValue::Object(map) => map
            .into_iter()
            .map(|(key, item)| {
                let value = match field_types.get(&key) {
                    Some(type_hint) => decode_json_for_type(item, type_hint, tree),
                    None => targetless_json_to_value(item),
                }?;
                Ok((key, value))
            })
            .collect::<DecodeResult<Vec<_>>>()
            .map(Value::dict),
        other => targetless_json_to_value(other),
    }
}

fn decode_tagged_enum(
    json: JsonValue,
    type_hint: &TypeNode,
    def: &SchemaDef,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    let enum_name = def.name.clone().unwrap_or_else(|| type_key(type_hint));
    match json {
        JsonValue::String(name) => decode_unit_variant(name, def, &enum_name),
        JsonValue::Object(map) => {
            if map.len() != 1 {
                return fail(format!(
                    "enum `{enum_name}` expects an externally tagged object with exactly one variant key"
                ));
            }
            let (name, payload) = map.into_iter().next().expect("len checked above");
            let matching: Vec<_> = def
                .variants
                .iter()
                .filter(|variant| variant.name == name)
                .collect();
            match matching.as_slice() {
                [variant] => {
                    decode_variant_payload(payload, type_hint, def, variant, &enum_name, tree)
                }
                [] => fail(format!(
                    "object tag `{name}` does not name a variant of enum `{enum_name}`"
                )),
                _ => fail(format!(
                    "object tag `{name}` is ambiguous for enum `{enum_name}`"
                )),
            }
        }
        other => targetless_json_to_value(other),
    }
}

fn decode_unit_variant(name: String, def: &SchemaDef, enum_name: &str) -> DecodeResult<Value> {
    let matching: Vec<_> = def
        .variants
        .iter()
        .filter(|variant| variant.name == name)
        .collect();
    match matching.as_slice() {
        [variant] if variant.fields.is_empty() => Ok(Value::variant_dict(
            Vec::<(String, Value)>::new(),
            variant.name.clone(),
            enum_name.to_string(),
        )),
        [variant] => fail(format!(
            "string `{name}` names enum variant `{}` but that variant requires payload fields",
            variant.name
        )),
        [] => fail(format!(
            "string `{name}` does not name a unit variant of enum `{enum_name}`"
        )),
        _ => fail(format!(
            "string `{name}` is ambiguous for enum `{enum_name}`"
        )),
    }
}

fn decode_variant_payload(
    payload: JsonValue,
    type_hint: &TypeNode,
    def: &SchemaDef,
    variant: &EnumVariant,
    enum_name: &str,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    if variant.fields.is_empty() {
        return match payload {
            JsonValue::Object(map) if map.is_empty() => Ok(Value::variant_dict(
                Vec::<(String, Value)>::new(),
                variant.name.clone(),
                enum_name.to_string(),
            )),
            other => fail(format!(
                "variant `{}` is a unit variant; expected empty object payload, got {}",
                variant.name,
                json_kind(&other)
            )),
        };
    }
    if let Some(fields) = tuple_variant_fields(variant) {
        decode_tuple_variant(payload, type_hint, def, variant, enum_name, tree, &fields)
    } else {
        decode_struct_variant(payload, type_hint, def, variant, enum_name, tree)
    }
}

fn decode_struct_variant(
    payload: JsonValue,
    type_hint: &TypeNode,
    def: &SchemaDef,
    variant: &EnumVariant,
    enum_name: &str,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    let JsonValue::Object(mut map) = payload else {
        return fail(format!(
            "variant `{}` expects object payload, got {}",
            variant.name,
            json_kind(&payload)
        ));
    };
    let mut values = Vec::with_capacity(variant.fields.len());
    for field in &variant.fields {
        let item = map.remove(&field.name).ok_or_else(|| {
            JsonDecodeError::new(format!(
                "enum variant `{}` is missing field `{}`",
                variant.name, field.name
            ))
        })?;
        let field_type = variant_field_type(field, def, type_hint).ok_or_else(|| {
            JsonDecodeError::new(format!(
                "enum variant `{}` field `{}` has no static type",
                variant.name, field.name
            ))
        })?;
        values.push((
            field.name.clone(),
            decode_json_for_type(item, &field_type, tree)?,
        ));
    }
    if !map.is_empty() {
        let mut names: Vec<_> = map.into_iter().map(|(name, _)| name).collect();
        names.sort();
        return fail(format!(
            "enum variant `{}` received unknown field(s): {}",
            variant.name,
            names.join(", ")
        ));
    }
    Ok(Value::variant_dict(
        values,
        variant.name.clone(),
        enum_name.to_string(),
    ))
}

fn decode_tuple_variant(
    payload: JsonValue,
    type_hint: &TypeNode,
    def: &SchemaDef,
    variant: &EnumVariant,
    enum_name: &str,
    tree: &AnalyzedTree,
    fields: &[&SchemaFieldDef],
) -> DecodeResult<Value> {
    let JsonValue::Array(items) = payload else {
        return fail(format!(
            "variant `{}` expects array payload, got {}",
            variant.name,
            json_kind(&payload)
        ));
    };
    if items.len() != fields.len() {
        return fail(format!(
            "variant `{}` expects {} payload item(s), got {}",
            variant.name,
            fields.len(),
            items.len()
        ));
    }
    let values = items
        .into_iter()
        .zip(fields)
        .map(|(item, field)| {
            let value = match variant_field_type(field, def, type_hint) {
                Some(type_hint) => decode_json_for_type(item, &type_hint, tree),
                None => targetless_json_to_value(item),
            }?;
            Ok((field.name.clone(), value))
        })
        .collect::<DecodeResult<Vec<_>>>()?;
    Ok(Value::variant_dict(
        values,
        variant.name.clone(),
        enum_name.to_string(),
    ))
}

fn tuple_variant_fields(variant: &EnumVariant) -> Option<Vec<&SchemaFieldDef>> {
    let mut indexed = variant
        .fields
        .iter()
        .map(|field| Some((field.name.parse::<usize>().ok()?, field)))
        .collect::<Option<Vec<_>>>()?;
    indexed.sort_by_key(|(index, _)| *index);
    indexed
        .iter()
        .enumerate()
        .all(|(expected, (actual, _))| expected == *actual)
        .then(|| indexed.into_iter().map(|(_, field)| field).collect())
}

fn variant_field_type(
    field: &SchemaFieldDef,
    def: &SchemaDef,
    type_hint: &TypeNode,
) -> Option<TypeNode> {
    field
        .type_hint
        .as_ref()
        .map(|field_type| substitute_type(field_type, &def.generics, &type_hint.generics))
}

fn decode_enum_union(
    json: JsonValue,
    type_hint: &TypeNode,
    tree: &AnalyzedTree,
) -> DecodeResult<Value> {
    let JsonValue::Array(items) = &json else {
        return targetless_json_to_value(json);
    };
    let matching: Vec<_> = type_hint
        .generics
        .iter()
        .filter(|alternative| type_accepts_tuple(alternative, items.len(), tree))
        .collect();
    match matching.as_slice() {
        [alternative] => decode_json_for_type(json, alternative, tree),
        _ => targetless_json_to_value(json),
    }
}

fn type_accepts_tuple(type_hint: &TypeNode, arity: usize, tree: &AnalyzedTree) -> bool {
    if type_key(type_hint) == "Tuple" {
        type_hint.generics.len() == arity
    } else {
        matches!(schema_tuple_elements(type_hint, tree), Some(elements) if elements.len() == arity)
    }
}

fn targetless_json_to_value(json: JsonValue) -> DecodeResult<Value> {
    if json.is_null() {
        return fail("JSON null needs an Option<T> target type");
    }
    serde_json::from_value(json)
        .map_err(|error| JsonDecodeError::new(format!("invalid JSON value: {error}")))
}

fn schema_tuple_elements(type_hint: &TypeNode, tree: &AnalyzedTree) -> Option<Vec<TypeNode>> {
    if let Some(def) = local_schema_def(type_hint, tree) {
        if let Some(elements) = &def.tuple_elements {
            return Some(substitute_types(
                elements,
                &def.generics,
                &type_hint.generics,
            ));
        }
    }
    let key = type_key(type_hint);
    let imports = tree.workspace_import_index.as_ref()?;
    let elements = imports.imported_tuple_schemas.get(&key)?;
    let generics = imports
        .imported_schema_generics
        .get(&key)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Some(substitute_types(elements, generics, &type_hint.generics))
}

fn schema_fields(type_hint: &TypeNode, tree: &AnalyzedTree) -> Option<HashMap<String, TypeNode>> {
    if let Some(def) = local_schema_def(type_hint, tree) {
        let fields = def
            .fields
            .iter()
            .filter_map(|field| Some((field.name.clone(), field.type_hint.as_ref()?.clone())));
        return Some(substitute_fields(
            fields,
            &def.generics,
            &type_hint.generics,
        ));
    }
    let key = type_key(type_hint);
    let imports = tree.workspace_import_index.as_ref()?;
    let fields = imports.imported_schemas.get(&key)?;
    let generics = imports
        .imported_schema_generics
        .get(&key)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Some(substitute_fields(
        fields
            .iter()
            .map(|(name, type_hint)| (name.clone(), type_hint.clone())),
        generics,
        &type_hint.generics,
    ))
}

fn local_schema_def<'a>(type_hint: &TypeNode, tree: &'a AnalyzedTree) -> Option<&'a SchemaDef> {
    if type_hint.path.len() != 1 {
        return None;
    }
    let name = &type_hint.path[0];
    let declaration = tree.root_schemas.iter().find(|decl| &decl.name == name)?;
    tree.schemas.get(&declaration.schema_node.id)
}

fn schema_def_for_type<'a>(type_hint: &TypeNode, tree: &'a AnalyzedTree) -> Option<&'a SchemaDef> {
    local_schema_def(type_hint, tree).or_else(|| {
        tree.workspace_import_index
            .as_ref()?
            .imported_schema_defs
            .get(&type_key(type_hint))
    })
}

fn substitute_fields(
    fields: impl Iterator<Item = (String, TypeNode)>,
    generic_names: &[String],
    concrete_args: &[TypeNode],
) -> HashMap<String, TypeNode> {
    fields
        .map(|(name, type_hint)| {
            (
                name,
                substitute_type(&type_hint, generic_names, concrete_args),
            )
        })
        .collect()
}

fn substitute_types(
    types: &[TypeNode],
    generic_names: &[String],
    concrete_args: &[TypeNode],
) -> Vec<TypeNode> {
    types
        .iter()
        .map(|type_hint| substitute_type(type_hint, generic_names, concrete_args))
        .collect()
}

fn substitute_type(
    type_hint: &TypeNode,
    generic_names: &[String],
    concrete_args: &[TypeNode],
) -> TypeNode {
    let substitutions: HashMap<String, TypeNode> = generic_names
        .iter()
        .cloned()
        .zip(concrete_args.iter().cloned())
        .collect();
    if substitutions.is_empty() {
        type_hint.clone()
    } else {
        substitute_generics_in_typenode(type_hint, &substitutions)
    }
}

fn type_key(type_hint: &TypeNode) -> String {
    type_hint.path.join(".")
}

fn json_kind(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_analyzer::analyze;
    use relon_parser::parse_document;
    use serde_json::json;

    fn analyzed(source: &str) -> AnalyzedTree {
        analyze(&parse_document(source).expect("parse"))
    }

    #[test]
    fn main_args_decode_option_and_schema_fields() {
        let tree = analyzed(
            r#"
            #schema Input { Option<Int> count: *, List<String> names: * }
            #main(Input input)
            input
            "#,
        );
        let args = decode_main_args(
            json!({"input": {"count": null, "names": ["a", "b"]}}),
            tree.main_signature.as_ref().expect("main signature"),
            &tree,
        )
        .expect("typed args");
        let Value::Dict(input) = args.get("input").expect("input") else {
            panic!("expected dict")
        };
        assert!(input.map.get("count").is_some_and(Value::is_option_none));
        assert!(matches!(input.map.get("names"), Some(Value::List(_))));
    }

    #[test]
    fn targetless_null_stays_rejected() {
        let tree = analyzed("#main(Int n) n");
        let error = decode_main_args(
            json!({"extra": null}),
            tree.main_signature.as_ref().expect("main signature"),
            &tree,
        )
        .expect_err("targetless null must fail");
        assert!(error.message().contains("Option<T>"));
    }

    #[test]
    fn tagged_enum_struct_payload_is_type_directed() {
        let tree = analyzed(
            r#"
            #enum Event { Started { id: String }, Stopped }
            #main(Event event)
            event
            "#,
        );
        let args = decode_main_args(
            json!({"event": {"Started": {"id": "job-1"}}}),
            tree.main_signature.as_ref().expect("main signature"),
            &tree,
        )
        .expect("enum args");
        let Value::Dict(event) = args.get("event").expect("event") else {
            panic!("expected variant dict")
        };
        assert_eq!(event.brand.as_deref(), Some("Started"));
        assert_eq!(event.variant_of.as_deref(), Some("Event"));
    }
}
