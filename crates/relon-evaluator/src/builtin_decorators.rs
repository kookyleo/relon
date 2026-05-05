//! Built-in [`DecoratorPlugin`] implementations.
//!
//! These plugins reproduce the language's first-class decorators (`@import`,
//! `@schema`, `@expect`, `@default`, `@value`, etc.) using the same extension
//! points exposed to host code. Every entry is registered in
//! [`crate::eval::Context::new`] alongside the standard library so user
//! programs see them as built-ins; hosts can override by registering their
//! own plugin under the same name first.

use crate::decorator::{DecoratorPlugin, PreEvalOutcome};
use crate::error::RuntimeError;
use crate::eval::{Context, Evaluator};
use crate::native_fn::EvaluatedArg;
use crate::scope::Scope;
use crate::value::{SchemaField, Value};
use relon_parser::{CallArg, Node, TokenRange};
use std::sync::Arc;

pub(crate) fn register_to(ctx: &mut Context) {
    ctx.register_decorator("import", Arc::new(ImportDecorator));
    ctx.register_decorator("schema", Arc::new(SchemaDecorator));
    ctx.register_decorator("expect", Arc::new(MessageDecorator));
    ctx.register_decorator("msg", Arc::new(MessageDecorator));
    ctx.register_decorator("error", Arc::new(MessageDecorator));
    ctx.register_decorator("default", Arc::new(DefaultDecorator));
    ctx.register_decorator("value", Arc::new(ValueDecorator));
}

/// `@import("path", as="alias", spread=false)` — injects module bindings into
/// the surrounding scope.
struct ImportDecorator;

impl DecoratorPlugin for ImportDecorator {
    fn pre_eval(
        &self,
        eval: &Evaluator<'_>,
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
        eval: &Evaluator<'_>,
        node: &Node,
        scope: &Arc<Scope>,
        _args: &[CallArg],
        _range: TokenRange,
    ) -> Result<PreEvalOutcome, RuntimeError> {
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
                return Ok(PreEvalOutcome::Override(Box::new(Value::Schema(fields))));
            }
        }

        // Slow path: host didn't attach an analyzer. Lower the body
        // on-demand using the same pure function the analyzer uses.
        // Diagnostics are dropped here — they're a host-facing feature
        // and there's no host channel to surface them on.
        let (lowered, _diags) = relon_analyzer::lower_schema_pure(None, node);
        if let Some(def) = lowered {
            if !def.variants.is_empty() {
                return Ok(PreEvalOutcome::Override(Box::new(build_enum_schema(
                    eval, &def, scope,
                )?)));
            }
            let fields = eval.build_schema_from_def(&def, scope)?;
            Ok(PreEvalOutcome::Override(Box::new(Value::Schema(fields))))
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
    _eval: &Evaluator<'_>,
    def: &relon_analyzer::SchemaDef,
    _scope: &Arc<Scope>,
) -> Result<Value, RuntimeError> {
    use std::collections::HashMap;
    let mut variants = HashMap::new();
    for variant in &def.variants {
        let mut fields = HashMap::new();
        for f in &variant.fields {
            let type_node = f.type_hint.clone().unwrap_or_else(|| relon_parser::TypeNode {
                path: vec!["Any".into()],
                generics: Vec::new(),
                is_optional: false,
                range: f.value_range,
                variant_fields: None,
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
        _eval: &Evaluator<'_>,
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
        _eval: &Evaluator<'_>,
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

/// `@value(replacement)` — substitutes the decorated value with the first
/// argument (or returns the original when called bare).
struct ValueDecorator;

impl DecoratorPlugin for ValueDecorator {
    fn wrap(
        &self,
        _eval: &Evaluator<'_>,
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
