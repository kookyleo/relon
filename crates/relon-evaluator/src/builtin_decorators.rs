//! Built-in [`DecoratorPlugin`] implementations.
//!
//! These plugins reproduce the language's first-class decorators (`@import`,
//! `@schema`, `@expect`, `@default`, `@value`, etc.) using the same extension
//! points exposed to host code. Every entry is registered in
//! [`crate::eval::Context::new`] alongside the standard library so user
//! programs see them as built-ins; hosts can override by registering their
//! own plugin under the same name first.

use crate::decorator::{DecoratorPlugin, PreEvalOutcome};
use crate::decorator_names::{
    BRAND, DEFAULT, ERROR, EXPECT, IMPORT, INPUT, LIBRARY, MSG, PRIVATE, SCHEMA, VALUE,
};
use crate::error::RuntimeError;
use crate::eval::{Context, Evaluator};
use crate::native_fn::EvaluatedArg;
use crate::schema::format_type_node;
use crate::scope::Scope;
use crate::value::{SchemaField, Value};
use relon_parser::{
    is_builtin_type_name, type_node_from_brand_arg, CallArg, Node, TokenRange, TypeNode,
};
use std::sync::Arc;

pub(crate) fn register_to(ctx: &mut Context) {
    ctx.register_decorator(IMPORT, Arc::new(ImportDecorator));
    ctx.register_decorator(SCHEMA, Arc::new(SchemaDecorator));
    // `@input(name=SchemaRef)` is a root-level marker. The analyzer
    // collects every decoration into `AnalyzedTree::input_decls`; the
    // evaluator's `prepare_input` step then evaluates each `SchemaRef`,
    // builds a wrapper schema `{ <name>: <schema> }`, and validates the
    // host-pushed `Context::with_input(...)` value against it before
    // walking the document body. The plugin slot here is identity —
    // the real work happens in those two passes.
    ctx.register_decorator(INPUT, Arc::new(InputDecorator));
    ctx.register_decorator(EXPECT, Arc::new(MessageDecorator));
    ctx.register_decorator(MSG, Arc::new(MessageDecorator));
    ctx.register_decorator(ERROR, Arc::new(MessageDecorator));
    ctx.register_decorator(DEFAULT, Arc::new(DefaultDecorator));
    ctx.register_decorator(VALUE, Arc::new(ValueDecorator));
    ctx.register_decorator(BRAND, Arc::new(BrandDecorator));
    // `@library` is a file-role marker consumed by the analyzer; the
    // evaluator only sees it when a library file is loaded as a module,
    // where it must behave as identity instead of tripping the
    // unknown-decorator fallback.
    ctx.register_decorator(LIBRARY, Arc::new(LibraryDecorator));
    // `@private` is a visibility marker. The dict-literal evaluator
    // checks for it directly (drops the field from the produced
    // `Value::Dict::map`); the plugin slot here exists only so the
    // unknown-decorator fallback doesn't fire.
    ctx.register_decorator(PRIVATE, Arc::new(PrivateDecorator));
}

/// `@import("path", as="alias", spread=false)` — injects module bindings into
/// the surrounding scope.
struct ImportDecorator;

impl DecoratorPlugin for ImportDecorator {
    fn pre_eval(
        &self,
        eval: &Evaluator,
        _node: &Node,
        scope: &Arc<Scope>,
        args: &[CallArg],
        range: TokenRange,
    ) -> Result<PreEvalOutcome, RuntimeError> {
        let new_scope = eval.apply_import(args, scope, range)?;
        Ok(PreEvalOutcome::Rescope(new_scope))
    }
}

/// `@schema` — interprets the decorated dict (or `Schema + Schema` / `Schema +
/// Dict` composition) as a schema definition rather than as data.
struct SchemaDecorator;

impl DecoratorPlugin for SchemaDecorator {
    fn pre_eval(
        &self,
        eval: &Evaluator,
        node: &Node,
        scope: &Arc<Scope>,
        args: &[CallArg],
        _range: TokenRange,
    ) -> Result<PreEvalOutcome, RuntimeError> {
        // Root-decorator form `@schema(Name={...})` is layout sugar for
        // co-locating schema declarations with `@input(...)`. The
        // analyzer's `collect_root_schemas` pass collects each named arg
        // into `tree.root_schemas`, and `Evaluator::seed_root_schemas`
        // registers them into the outer scope before the body walk.
        // Here on the decorator runtime side we are a no-op: the
        // decorated node (the root dict) is *not* a schema body — it's
        // ordinary data. Falling through to the schema-lowering path
        // would try to interpret the whole document as a schema.
        if !args.is_empty() {
            return Ok(PreEvalOutcome::Pass);
        }
        // Fast path: an attached `AnalyzedTree` already split this
        // body into typed fields. Build the runtime `Value::Schema`
        // directly from the pre-computed `SchemaDef`, doing only the
        // work that genuinely requires the live scope.
        if let Some(tree) = eval.context.analyzed.as_ref() {
            if let Some(def) = tree.schema(node.id) {
                if !def.variants.is_empty() {
                    return Ok(PreEvalOutcome::Override(Box::new(build_enum_schema(
                        eval, def, scope,
                    )?)));
                }
                let fields = eval.build_schema_from_def(def, scope)?;
                return Ok(PreEvalOutcome::Override(Box::new(Value::Schema {
                    generics: def.generics.clone(),
                    fields,
                })));
            }
        }

        // Slow path: host didn't attach an analyzer. Lower the body
        // on-demand using the same pure function the analyzer uses.
        // Diagnostics are dropped here — they're a host-facing feature
        // and there's no host channel to surface them on.
        let (lowered, _diags) = relon_analyzer::lower_schema_pure(None, Vec::new(), node);
        if let Some(def) = lowered {
            if !def.variants.is_empty() {
                return Ok(PreEvalOutcome::Override(Box::new(build_enum_schema(
                    eval, &def, scope,
                )?)));
            }
            let fields = eval.build_schema_from_def(&def, scope)?;
            Ok(PreEvalOutcome::Override(Box::new(Value::Schema {
                generics: def.generics.clone(),
                fields,
            })))
        } else {
            // The lowering rejected the body shape; let default
            // evaluation take over (typical case: `@schema` placed on
            // a value that the user expects to evaluate to a Schema
            // already).
            Ok(PreEvalOutcome::Pass)
        }
    }
}

/// Build a runtime `Value::EnumSchema` from a sum-type `SchemaDef`. Each
/// variant becomes its own `HashMap<String, SchemaField>` so the variant
/// constructor path can validate body shapes the same way `apply_schema`
/// does for plain dicts.
fn build_enum_schema(
    _eval: &Evaluator,
    def: &relon_analyzer::SchemaDef,
    _scope: &Arc<Scope>,
) -> Result<Value, RuntimeError> {
    use std::collections::HashMap;
    let mut variants = HashMap::new();
    for variant in &def.variants {
        let mut fields = HashMap::new();
        for f in &variant.fields {
            let type_node = f
                .type_hint
                .clone()
                .unwrap_or_else(|| relon_parser::TypeNode {
                    path: vec!["Any".into()],
                    generics: Vec::new(),
                    is_optional: false,
                    range: f.value_range,
                    variant_fields: None,
                    doc_comment: None,
                });
            fields.insert(
                f.name.clone(),
                SchemaField {
                    type_hint: type_node,
                    predicates: vec![Value::Wildcard],
                    custom_error: None,
                    default_value: None,
                },
            );
        }
        variants.insert(variant.name.clone(), fields);
    }
    let name = def.name.clone().unwrap_or_default();
    Ok(Value::EnumSchema { name, variants })
}

/// `@expect("message")` / `@msg(...)` / `@error(...)` — attaches a custom error
/// message to a schema field; identity when applied to ordinary values.
struct MessageDecorator;

impl DecoratorPlugin for MessageDecorator {
    fn schema_field_meta(
        &self,
        _eval: &Evaluator,
        field: &mut SchemaField,
        _scope: &Arc<Scope>,
        args: &[EvaluatedArg],
        _range: TokenRange,
    ) -> Result<(), RuntimeError> {
        if let Some(arg) = args.first() {
            field.custom_error = Some(arg.value.to_string());
        }
        Ok(())
    }
}

/// `@default(value)` — supplies a default value for a missing schema field;
/// identity when applied to ordinary values.
struct DefaultDecorator;

impl DecoratorPlugin for DefaultDecorator {
    fn schema_field_meta(
        &self,
        _eval: &Evaluator,
        field: &mut SchemaField,
        _scope: &Arc<Scope>,
        args: &[EvaluatedArg],
        _range: TokenRange,
    ) -> Result<(), RuntimeError> {
        if let Some(arg) = args.first() {
            field.default_value = Some(arg.value.clone());
        }
        Ok(())
    }
}

/// `@library` — file-role marker. All hooks are identity; the real effect
/// lives in `relon-analyzer` (sets `AnalyzedTree::is_library`) and the
/// `relon` facade (refuses library files as evaluation entries).
struct LibraryDecorator;

impl DecoratorPlugin for LibraryDecorator {}

/// `@private` — dict-field visibility marker. All plugin hooks are
/// identity; the real effect (dropping the field from the produced
/// `Value::Dict::map`) is implemented inline in the dict-literal
/// evaluator. Registered here so the unknown-decorator fallback
/// doesn't fire when the user writes `@private` on a field.
struct PrivateDecorator;

impl DecoratorPlugin for PrivateDecorator {}

/// `@input(name=SchemaRef)` — root-level marker for input slots. The
/// effect lives in `relon-analyzer::inputs::collect_inputs` (records
/// the slot) and `Evaluator::prepare_input` (builds the wrapper schema
/// and validates the host-pushed value). Registered here so the
/// unknown-decorator fallback doesn't fire when the user writes
/// `@input(user=User)` on the root dict.
struct InputDecorator;

impl DecoratorPlugin for InputDecorator {}

/// `@value(replacement)` — substitutes the decorated value with the first
/// argument (or returns the original when called bare).
struct ValueDecorator;

impl DecoratorPlugin for ValueDecorator {
    fn wrap(
        &self,
        _eval: &Evaluator,
        value: Value,
        _scope: &Arc<Scope>,
        args: &[EvaluatedArg],
        _range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if let Some(arg) = args.first() {
            Ok(arg.value.clone())
        } else {
            Ok(value)
        }
    }
}

/// `@brand(Type)` — decorator-position mirror of the field-level type hint
/// (`Type field: { ... }`). The first positional argument is parsed as a
/// **type expression**, not an ordinary value: a bareword (`@brand(Weather)`)
/// or a string literal (`@brand("Weather")`) both resolve to the type name
/// `"Weather"`. Dotted paths (`@brand(geo.Location)`) are also accepted.
///
/// Apply rules (kept in lockstep with the field-level path in
/// `Evaluator::eval_internal`):
///
/// * On a `Dict`: runs `check_type` against the named schema (if registered)
///   and writes `dict.brand = Some(name)`. Built-in type names (`Int`,
///   `String`, ...) only validate; brand is not stored — same as the field
///   form.
/// * On a non-`Dict`: runs `check_type` only. Brand has nowhere to live.
/// * Conflict: if the host node also carries a field-level type hint, the
///   user has expressed the same intent twice; refuse with a clear error.
struct BrandDecorator;

impl DecoratorPlugin for BrandDecorator {
    fn wrap_with_ast(
        &self,
        eval: &Evaluator,
        node: &Node,
        value: &Value,
        scope: &Arc<Scope>,
        ast_args: &[CallArg],
        range: TokenRange,
    ) -> Result<Option<Value>, RuntimeError> {
        // Reject the ambiguous `Foo x: @brand(Bar) {...}` form up front. The
        // outer `Foo` hint and the inner `@brand` would both try to write
        // `dict.brand` (and run their own `check_type`); it's almost never
        // what the author meant.
        if node.type_hint.is_some() {
            return Err(RuntimeError::UnsupportedOperator(
                "@brand cannot be combined with a field-level type hint on the same value; pick one"
                    .to_string(),
                range,
            ));
        }

        let arg = ast_args.first().ok_or_else(|| {
            RuntimeError::UnsupportedOperator(
                "@brand requires a type argument, e.g. @brand(Weather)".to_string(),
                range,
            )
        })?;
        if arg.name.is_some() {
            return Err(RuntimeError::UnsupportedOperator(
                "@brand does not accept named arguments; use @brand(Type)".to_string(),
                range,
            ));
        }

        let type_node = type_node_from_brand_arg(&arg.value.expr, range).ok_or_else(|| {
            RuntimeError::UnsupportedOperator(
                "@brand argument must be a type name (bareword, string, dotted path, or generic type)"
                    .to_string(),
                range,
            )
        })?;

        let mut new_val = value.clone();
        // Wildcard short-circuits in the field path; preserve that here so
        // the two entry points stay observationally equivalent.
        if !matches!(new_val, Value::Wildcard) {
            eval.check_type(&mut new_val, &type_node, scope, range)?;

            if let Value::Dict(ref mut d) = new_val {
                let d = Arc::make_mut(d);
                d.brand = brand_string_for(&type_node);
            }
        }

        Ok(Some(new_val))
    }
}

/// Compute the brand string written into `dict.brand` for a given type
/// reference. Mirrors the field-form rule (`type_hint` path) and extends it
/// to cover generic / optional shapes so `@brand(Map<String, Int>)`,
/// `@brand(Foo<T>)`, and `@brand(Weather?)` all produce a brand that round-
/// trips through `Type` match arms and JSON output.
///
/// * Single segment built-in (`Int`, `String`, …) without generics or `?` →
///   `None` (built-ins never carry an identity brand, same as the field form).
/// * Single segment custom type without generics or `?` → just the name
///   (`Some("Weather")`).
/// * Anything else → `format_type_node`-serialized string
///   (`Some("Map<String, Int>")`, `Some("Weather?")`, `Some("geo.Location")`).
pub(crate) fn brand_string_for(type_node: &TypeNode) -> Option<String> {
    if type_node.generics.is_empty() && !type_node.is_optional && type_node.path.len() == 1 {
        let tname = &type_node.path[0];
        if is_builtin_type_name(tname) {
            return None;
        }
        return Some(tname.clone());
    }
    Some(format_type_node(type_node))
}
