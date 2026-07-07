//! Decorator plugin protocol.
//!
//! Hosts extend Relon's `@name(...)` syntax by implementing
//! [`DecoratorPlugin`] and registering an instance under the decorator's
//! full dotted path name. Decorator hooks run *inside* a tree-walk
//! evaluation, so each hook receives a [`crate::TreeWalkEval`] handle —
//! the fragment surface (`eval` / `force_thunk` / `invoke_closure`) a
//! plugin may need to evaluate argument nodes or call closures mid-hook.
//! The trait lives in `relon-eval-api` so plugins need no dependency on a
//! backend crate; the built-in decorators (`#import`, `#schema`,
//! `#ensure`, ...) and any host-side plugin live in their respective
//! backend / host crates.

use crate::error::RuntimeError;
use crate::native_fn::EvaluatedArg;
use crate::scope::Scope;
use crate::value::{SchemaField, Value};
use crate::TreeWalkEval;
use relon_parser::{CallArg, Node, TokenRange};
use std::sync::Arc;

/// The outcome of a decorator's pre-evaluation hook.
///
/// A decorator runs *before* the node it annotates is evaluated. It can
/// either step aside, swap the active scope, or take over the value entirely.
///
/// `Override` boxes its `Value` to keep the enum small — `Value` is by far
/// the largest variant payload and most decorators choose `Pass`.
pub enum PreEvalOutcome {
    /// Run the default evaluation path with the existing scope.
    Pass,

    /// Run the default evaluation path, but swap in this scope first.
    /// Used by `#import` to inject the imported module's bindings.
    Rescope(Arc<Scope>),

    /// Skip the default evaluation path; use this value as the result.
    /// Used by `#schema` to interpret the body as a schema definition
    /// rather than as data.
    Override(Box<Value>),
}

/// Hosts extend Relon's `@name(...)` syntax by implementing this trait and
/// registering an instance under the decorator's full dotted path name (e.g.
/// `"import"`, `"ensure.int"`, `"my_org.audit"`).
///
/// Three independent hooks are exposed:
///
/// * [`pre_eval`](Self::pre_eval) runs before the decorated node is evaluated.
///   Use it to inject locals into scope (`#import`) or take over the value
///   entirely (`#schema`).
/// * [`wrap`](Self::wrap) runs after the node is evaluated. Use it to validate
///   or transform the value (`@ensure.int`, `@currency("USD")`).
/// * [`schema_field_meta`](Self::schema_field_meta) runs while extracting
///   fields from a `#schema`-annotated dict. Use it to attach per-field
///   metadata such as defaults or custom error messages (`#expect`, `#default`).
///
/// All hooks default to no-op / identity, so plugins only override what they
/// actually need.
pub trait DecoratorPlugin: Send + Sync {
    fn pre_eval(
        &self,
        _eval: &dyn TreeWalkEval,
        _node: &Node,
        _scope: &Arc<Scope>,
        _args: &[CallArg],
        _range: TokenRange,
    ) -> Result<PreEvalOutcome, RuntimeError> {
        Ok(PreEvalOutcome::Pass)
    }

    fn wrap(
        &self,
        _eval: &dyn TreeWalkEval,
        value: Value,
        _scope: &Arc<Scope>,
        _args: &[EvaluatedArg],
        _range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        Ok(value)
    }

    /// AST-level wrap hook. Called *before* [`Self::wrap`]: receives the
    /// decorator's raw [`CallArg`] AST nodes (un-evaluated) along with the
    /// host [`Node`] so plugins that need to inspect type expressions,
    /// paths, or sibling decorators / type hints can do so without losing
    /// them to evaluation.
    ///
    /// Returns `Ok(None)` to fall through to the standard `wrap` path.
    /// Returning `Ok(Some(val))` short-circuits: `wrap` is not called and
    /// `val` becomes the decorator's output. `@brand(Type)` uses this to
    /// extract the type name from the un-evaluated AST so a bareword like
    /// `Weather` doesn't get resolved to a `Value::Schema` first.
    fn wrap_with_ast(
        &self,
        _eval: &dyn TreeWalkEval,
        _node: &Node,
        _value: &Value,
        _scope: &Arc<Scope>,
        _ast_args: &[CallArg],
        _range: TokenRange,
    ) -> Result<Option<Value>, RuntimeError> {
        Ok(None)
    }

    fn schema_field_meta(
        &self,
        _eval: &dyn TreeWalkEval,
        _field: &mut SchemaField,
        _scope: &Arc<Scope>,
        _args: &[EvaluatedArg],
        _range: TokenRange,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}
