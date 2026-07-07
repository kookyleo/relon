//! Built-in [`DecoratorPlugin`] implementations.
//!
//! The registry holds two kinds of entries:
//!
//! * The single `@`-decorator: `@value` (host-registered value transform).
//! * Schema-field meta directives — `#default`, `#expect`, `#msg`,
//!   `#error` — registered under their directive names so the evaluator's
//!   `schema_field_meta` dispatcher (which only looks at the name) can
//!   reach them. They have no `pre_eval` / `wrap` impact on ordinary
//!   values.
//!
//! Other directives (`#schema`, `#import`, `#brand`, `#internal`,
//! `#main`) are handled directly by the evaluator
//! (`apply_directive_pre` / `apply_directive_post`) and the analyzer's
//! collection passes; they don't appear in this registry.
//!
//! User-definable decorators (`@f`, `@f(args)` where `f` resolves to a
//! callable in scope) are handled by the fallback path in
//! `TreeWalkEvaluator::fallback_decorator`; no built-in registration entry is
//! needed for them.

use crate::decorator::DecoratorPlugin;
use crate::decorator_names::{DEFAULT, ERROR, EXPECT, MSG, VALUE};
use crate::error::RuntimeError;
use crate::native_fn::EvaluatedArg;
use crate::scope::Scope;
use crate::value::{SchemaField, Value};
use relon_analyzer::format_type;
use relon_eval_api::context::Context;
use relon_eval_api::TreeWalkEval;
use relon_parser::{is_builtin_type_name, TokenRange, TypeNode};
use std::sync::Arc;

pub(crate) fn register_to(ctx: &mut Context) {
    ctx.register_decorator(VALUE, Arc::new(ValueDecorator));
    ctx.register_decorator(EXPECT, Arc::new(MessageDecorator));
    ctx.register_decorator(MSG, Arc::new(MessageDecorator));
    ctx.register_decorator(ERROR, Arc::new(MessageDecorator));
    ctx.register_decorator(DEFAULT, Arc::new(DefaultDecorator));
}

/// `@value(replacement)` — substitutes the decorated value with the first
/// argument (or returns the original when called bare).
struct ValueDecorator;

impl DecoratorPlugin for ValueDecorator {
    fn wrap(
        &self,
        _eval: &dyn TreeWalkEval,
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

/// `#expect "msg"` / `#msg "msg"` / `#error "msg"` — attaches a custom
/// error message to a schema field; identity when applied to ordinary
/// values.
struct MessageDecorator;

impl DecoratorPlugin for MessageDecorator {
    fn schema_field_meta(
        &self,
        _eval: &dyn TreeWalkEval,
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

/// `#default <value>` — supplies a default value for a missing schema
/// field; identity when applied to ordinary values.
struct DefaultDecorator;

impl DecoratorPlugin for DefaultDecorator {
    fn schema_field_meta(
        &self,
        _eval: &dyn TreeWalkEval,
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

/// Compute the brand string written into `dict.brand` for a given type
/// reference. Mirrors the field-form rule (`type_hint` path) and extends it
/// to cover generic / optional shapes so `#brand Map<String, Int>`,
/// `#brand Foo<T>`, and `#brand Weather?` all produce a brand that round-
/// trips through `Type` match arms and JSON output.
///
/// * Single segment built-in (`Int`, `String`, …) without generics or `?` →
///   `None` (built-ins never carry an identity brand, same as the field form).
/// * Single segment custom type without generics or `?` → just the name
///   (`Some("Weather")`).
/// * Anything else → `format_type`-serialized string
///   (`Some("Map<String, Int>")`, `Some("Weather?")`, `Some("geo.Location")`).
pub(crate) fn brand_string_for(type_node: &TypeNode) -> Option<String> {
    if type_node.generics.is_empty() && !type_node.is_optional && type_node.path.len() == 1 {
        let tname = &type_node.path[0];
        if is_builtin_type_name(tname) {
            return None;
        }
        return Some(tname.clone());
    }
    Some(format_type(type_node))
}
