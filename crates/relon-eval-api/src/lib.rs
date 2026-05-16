//! Public, backend-agnostic surface for Relon evaluation.
//!
//! This crate is the seam between hosts and evaluator backends; it only
//! re-exports the types a caller actually sees:
//!
//! * Data shapes: [`Value`], [`Scope`], [`Thunk`], [`RuntimeError`].
//! * Host configuration surface: [`Context`], [`Capabilities`],
//!   [`NativeFnGate`], native-fn / decorator registration.
//! * Backend contract: the [`Evaluator`] trait — five `&self` methods
//!   covering one full evaluation lifecycle.
//!
//! A backend (tree-walking `relon_evaluator::TreeWalkEvaluator`, a future
//! bytecode VM, ...) implements this single trait; hosts then hold a
//! `Box<dyn Evaluator>` for dynamic dispatch / backend swap.
//!
//! Trait object-safety is a hard requirement: every method is `&self` plus
//! concrete in/out types — no generic methods.

#![forbid(unsafe_code)]
// rustc ≥ 1.93 false-positive: `unused_assignments` fires on fields of every
// `#[derive(miette::Diagnostic)]` / `thiserror::Error` enum (the derive
// expands to internal let-bindings that the lint mis-reads). Mirror the
// evaluator crate's allow and drop it once the rustc fix lands.
#![allow(unused_assignments)]

pub mod buffer;
pub mod context;
pub mod decorator;
pub mod error;
pub mod layout;
pub mod module;
pub mod native_fn;
pub mod schema_canonical;
pub mod schema_lower;
pub mod scope;
pub mod value;

pub use context::{
    Capabilities, CapabilityBit, Context, GatedNativeFn, LoadingModuleGuard, NativeFnGate,
};
pub use decorator::{DecoratorPlugin, PreEvalOutcome};
pub use error::RuntimeError;
pub use module::{ModuleResolver, ModuleSource};
pub use native_fn::{EvaluatedArg, NativeArgs, NativeFn, NativeFnCaps, RelonFunction};
pub use scope::{ListContext, Locals, RootRef, Scope, Thunk, Thunks};
pub use value::{ClosureData, EnumSchemaData, SchemaData, SchemaField, Value, ValueDict};

use std::collections::HashMap;
use std::sync::Arc;

/// Backend-agnostic evaluator contract.
///
/// Implementations turn an analyzed AST into a [`Value`]. The interface is
/// deliberately object-safe: every method is `&self` with concrete-type
/// arguments and return values — no generic methods — so hosts can hold a
/// `Box<dyn Evaluator>` for backend swap or dynamic dispatch.
///
/// The five methods cover one full evaluation lifecycle:
///
/// * [`eval`](Self::eval) — evaluate a single node (fragment / debug entry).
/// * [`eval_root`](Self::eval_root) — evaluate the document attached via
///   `Context::with_root` as a library / static config.
/// * [`run_main`](Self::run_main) — evaluate the document as an entry
///   program: check host `args` against the `#main(...)` signature, bind
///   them, then walk the body.
/// * [`force_thunk`](Self::force_thunk) — drive a lazy dict entry to a
///   value, caching the result for later accesses.
/// * [`invoke_closure`](Self::invoke_closure) — call a constructed closure
///   value with positional args; the shortest entry point for hosts that
///   treat Relon closures as plain callbacks.
pub trait Evaluator: Send + Sync {
    /// Evaluate a single AST node under `scope`.
    fn eval(&self, node: &relon_parser::Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError>;

    /// Evaluate the document attached via `Context::with_root` as a library
    /// / static config (no `#main(...)` consultation, no host args).
    fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError>;

    /// Evaluate the document as an entry program: check `args` against the
    /// file's `#main(...)` signature, bind them, then walk the body.
    /// Returns `NoMainSignature` if the file lacks `#main(...)`.
    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError>;

    /// Drive a lazy thunk to a value. The first call evaluates `thunk.node`
    /// under `thunk.scope` and caches the result; later calls return the
    /// cached value.
    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError>;

    /// Invoke a constructed closure value with positional `args`. The
    /// shortest entry point when a host wants to call a Relon closure as a
    /// plain callback.
    fn invoke_closure(&self, closure: &ClosureData, args: &[Value]) -> Result<Value, RuntimeError>;
}
