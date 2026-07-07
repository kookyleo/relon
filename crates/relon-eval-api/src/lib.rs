//! Public, backend-agnostic surface for Relon evaluation.
//!
//! This crate is the seam between hosts and evaluator backends; it
//! holds the types a caller actually sees:
//!
//! * Data shapes: [`Value`], [`Scope`], [`Thunk`], [`RuntimeError`],
//!   plus the [`SmolStr`] small-string type `Value` is built on.
//! * Host configuration surface: [`Context`], [`Capabilities`],
//!   [`NativeFnGate`], native-fn / decorator registration, module
//!   resolution ([`ModuleResolver`]).
//! * Policy boundary: the [`CapabilityGate`] trait — single source of
//!   capability-policy truth consulted by every backend (see
//!   `capability` module docs for the enforcement-timing diff
//!   between dispatch-time tree-walker checks and vtable-build-time
//!   cranelift checks).
//! * Backend contract: the [`Evaluator`] trait — the two-method
//!   backend-agnostic core (`eval_root` / `run_main`) — plus the
//!   [`TreeWalkEval`] extension trait for the AST-fragment surface
//!   (`eval` / `force_thunk` / `invoke_closure`) only tree-walking
//!   implementations can provide.
//!
//! The internal ABI the compiled backends share (binary handshake
//! buffer, record layout, schema canonicalisation, return-path
//! verifier) lives in the `relon-abi` crate, not here: hosts never
//! touch it, backends depend on it directly.
//!
//! Every backend (tree-walking `relon_evaluator::TreeWalkEvaluator`,
//! cranelift AOT, LLVM AOT, wasm host wrapper, ...) implements
//! [`Evaluator`]; hosts then hold a `Box<dyn Evaluator>` for dynamic
//! dispatch / backend swap. Only tree-walking implementations
//! additionally implement [`TreeWalkEval`].
//!
//! Trait object-safety is a hard requirement: every method is `&self` plus
//! concrete in/out types — no generic methods.

// `unsafe_code` is forbidden everywhere except the SSO `SmolStr` module,
// which needs a single `str::from_utf8_unchecked` on the inline-payload
// borrow to keep `as_str()` cost on par with `String::as_str()`. The
// invariant is local — every constructor fills `data[..len]` from a
// `&str` / `String` so the bytes are UTF-8 by construction — and the
// `unsafe` block has an explicit `// SAFETY:` comment documenting it.
#![deny(unsafe_code)]
// rustc ≥ 1.93 false-positive: `unused_assignments` fires on fields of every
// `#[derive(miette::Diagnostic)]` / `thiserror::Error` enum (the derive
// expands to internal let-bindings that the lint mis-reads). Mirror the
// evaluator crate's allow and drop it once the rustc fix lands.
#![allow(unused_assignments)]

pub mod capability;
pub mod context;
pub mod decorator;
pub mod error;
pub mod module;
pub mod native_fn;
pub mod scope;
pub mod smol_str;
pub mod value;

pub use capability::CapabilityGate;
pub use context::{
    Capabilities, CapabilityBit, Context, GatedNativeFn, LoadingModuleGuard, NativeFnGate,
    ResourceBudget, ResourceBudgetProfile, TopLevelRunGuard,
};
pub use decorator::{DecoratorPlugin, PreEvalOutcome};
pub use error::RuntimeError;
pub use module::{ModuleResolver, ModuleSource};
pub use native_fn::{EvaluatedArg, NativeArgs, NativeFn, NativeFnCaps, RelonFunction};
pub use scope::{ListContext, Locals, RootRef, Scope, Thunk, Thunks};
pub use smol_str::{SmolStr, SMOL_STR_INLINE_CAP};
pub use value::{ClosureData, EnumSchemaData, SchemaData, SchemaField, Value, ValueDict};

use std::collections::HashMap;
use std::sync::Arc;

/// Backend-agnostic evaluator contract.
///
/// Implementations turn a Relon document into a [`Value`]. The interface
/// is deliberately object-safe: every method is `&self` with concrete-type
/// arguments and return values — no generic methods — so hosts can hold a
/// `Box<dyn Evaluator>` for backend swap or dynamic dispatch.
///
/// This is the *portable core*: it promises only what every backend —
/// tree-walk interpreter, cranelift AOT, LLVM AOT, wasm host wrapper —
/// can honour from a source document alone:
///
/// * [`eval_root`](Self::eval_root) — evaluate the document attached via
///   `Context::with_root` as a library / static config.
/// * [`run_main`](Self::run_main) — evaluate the document as an entry
///   program: check host `args` against the `#main(...)` signature, bind
///   them, then run the body.
///
/// Capabilities that need live AST and environment access at run time —
/// fragment evaluation, thunk forcing, closure invocation — are **not**
/// part of this contract. They live on the [`TreeWalkEval`] extension
/// trait: compiled backends lower the AST to machine code up front and
/// discard it, so promising those methods here would put obligations on
/// implementors that they can never meet.
///
/// # Concurrency contract
///
/// `Send + Sync` + `&self` means an evaluator may be shared across
/// threads, but the **top-level** entry points ([`eval_root`](Self::eval_root)
/// and [`run_main`](Self::run_main)) reset per-run sandbox state that
/// lives on the shared [`Context`] — the step-budget counter, the
/// reference `path_cache`, and the iter-cursor table. Backends MUST
/// serialize those entry points per `Context` via
/// [`Context::begin_top_level_run`]; otherwise a concurrent run would
/// zero a mid-flight run's step accounting (`Capabilities::max_steps`
/// is a security boundary) and serve values cached under different
/// `#main` args. Under that protocol:
///
/// * Concurrent `eval_root` / `run_main` calls on the same `Context`
///   (through one evaluator or several sharing it) block until the
///   active run finishes — they never interleave.
/// * Re-entering `eval_root` / `run_main` from *within* a run on the
///   same thread (e.g. from a native-fn or decorator callback) panics
///   instead of self-deadlocking; nested work must use the
///   non-resetting entry points ([`TreeWalkEval::eval`],
///   [`TreeWalkEval::force_thunk`], [`TreeWalkEval::invoke_closure`],
///   or `NativeFnCaps::call_relon`), which are safe mid-run.
/// * Hosts that want genuinely parallel evaluation give each thread its
///   own `Context` + evaluator; contexts share nothing mutable.
pub trait Evaluator: Send + Sync {
    /// Evaluate the document attached via `Context::with_root` as a library
    /// / static config (no `#main(...)` consultation, no host args).
    fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError>;

    /// Evaluate the document as an entry program: check `args` against the
    /// file's `#main(...)` signature, bind them, then run the body.
    ///
    /// Tuple parameters must be supplied as `Value::Tuple` (or
    /// `Value::tuple(...)`). Targetless JSON decoding such as
    /// `serde_json::from_value::<Value>` maps JSON arrays to `Value::List`,
    /// which intentionally does not satisfy `Tuple<...>` parameters; JSON
    /// `null` is rejected unless a host boundary decodes it against an
    /// explicit `Option<T>` or `T?` target.
    ///
    /// Returns `NoMainSignature` if the file lacks `#main(...)`.
    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError>;
}

/// Tree-walk-only evaluation surface, layered over [`Evaluator`] the way
/// `BufRead` layers over `Read`.
///
/// These methods require the analyzed AST and a live scope / thunk /
/// closure environment at run time, so only backends that *interpret* the
/// tree can implement them: today the tree-walking interpreter
/// (`relon_evaluator::TreeWalkEvaluator`) and wrappers that embed one
/// (the facade's auto-tier evaluator). Compiled backends (cranelift AOT,
/// LLVM AOT) cannot — and therefore only implement the core [`Evaluator`].
///
/// The same object-safety rule applies: every method is `&self` with
/// concrete in/out types, so hosts and plugins can hold a
/// `&dyn TreeWalkEval` (decorator hooks receive exactly that).
///
/// These are the *non-resetting* entry points named in the concurrency
/// contract on [`Evaluator`]: they never reset per-run sandbox state, so
/// they are safe to call mid-run from native-fn and decorator callbacks.
pub trait TreeWalkEval: Evaluator {
    /// Evaluate a single AST node under `scope` (fragment / debug entry).
    fn eval(&self, node: &relon_parser::Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError>;

    /// Drive a lazy thunk to a value. The first call evaluates `thunk.node`
    /// under `thunk.scope` and caches the result; later calls return the
    /// cached value.
    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError>;

    /// Invoke a constructed closure value with positional `args`. The
    /// shortest entry point when a host wants to call a Relon closure as a
    /// plain callback.
    fn invoke_closure(&self, closure: &ClosureData, args: &[Value]) -> Result<Value, RuntimeError>;
}
