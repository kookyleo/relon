//! Public, backend-agnostic surface for Relon evaluation.
//!
//! This crate is the seam between hosts and evaluator backends; it only
//! re-exports the types a caller actually sees:
//!
//! * Data shapes: [`Value`], [`Scope`], [`Thunk`], [`RuntimeError`].
//! * Host configuration surface: [`Context`], [`Capabilities`],
//!   [`NativeFnGate`], native-fn / decorator registration.
//! * Policy boundary: the [`CapabilityGate`] trait — single source of
//!   capability-policy truth consulted by every backend (see
//!   `capability` module docs for the enforcement-timing diff
//!   between dispatch-time tree-walker checks and vtable-build-time
//!   cranelift checks).
//! * Backend contract: the [`Evaluator`] trait — five `&self` methods
//!   covering one full evaluation lifecycle.
//!
//! A backend (tree-walking `relon_evaluator::TreeWalkEvaluator`, a future
//! bytecode VM, ...) implements this single trait; hosts then hold a
//! `Box<dyn Evaluator>` for dynamic dispatch / backend swap.
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

pub mod buffer;
pub mod capability;
pub mod context;
pub mod decorator;
pub mod error;
pub mod inplace_return;
pub mod layout;
pub mod module;
pub mod native_fn;
pub mod schema_canonical;
pub mod schema_lower;
pub mod scope;
pub mod smol_str;
pub mod value;
pub mod verifier;

pub use capability::CapabilityGate;
pub use context::{
    Capabilities, CapabilityBit, Context, GatedNativeFn, LoadingModuleGuard, NativeFnGate,
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

    /// Drive a lazy thunk to a value. The first call evaluates `thunk.node`
    /// under `thunk.scope` and caches the result; later calls return the
    /// cached value.
    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError>;

    /// Invoke a constructed closure value with positional `args`. The
    /// shortest entry point when a host wants to call a Relon closure as a
    /// plain callback.
    fn invoke_closure(&self, closure: &ClosureData, args: &[Value]) -> Result<Value, RuntimeError>;

    /// v6-δ M1 R3: resume a deopt'd trace at the supplied IR-side
    /// `external_pc` with the SSA-slot snapshot the trace captured
    /// just before the guard fired.
    ///
    /// ## Contract
    ///
    /// - `args` carries the `#main(...)` arguments the trace was
    ///   originally invoked with. Hosts that lost the original args
    ///   (e.g. a guard fired before any LocalGet) MUST pass an empty
    ///   map; `MissingMainArg` then surfaces as today.
    /// - `external_pc` is the synthetic PC the recorder stamped on
    ///   the failing guard's `GuardSite`. For backends that do not
    ///   maintain an `external_pc → (block, ip)` table the trait
    ///   default discards the value and re-runs from entry — the
    ///   4-prong sandbox semantics still hold because every trap
    ///   surface re-fires on the re-run.
    /// - `local_snapshot` is a flat `&[u64]` containing
    ///   `DeoptStateSnapshot::ssa_slots_copy`. Backends that can
    ///   round-trip the slots back to scope locals MAY do so for
    ///   pixel-perfect partial-resume; the default ignores them.
    ///
    /// ## Default implementation
    ///
    /// The default — used by the `TreeWalkEvaluator` — drops both
    /// `external_pc` and `local_snapshot` and forwards to
    /// [`Self::run_main`]. This is the v6-γ M5 fallback semantic
    /// promoted to the trait surface so the trace-install path can
    /// always reach it via a single trait method.
    ///
    /// A future bytecode VM backend that exposes an IR-level
    /// `(block, ip)` map can override this to honour the PC and
    /// rehydrate `local_snapshot` into its frame — at that point the
    /// host dispatcher gets the full "deopt to the exact next IR op"
    /// semantics without changing the trait surface.
    fn resume_from_pc(
        &self,
        args: HashMap<String, Value>,
        external_pc: u64,
        local_snapshot: &[u64],
    ) -> Result<Value, RuntimeError> {
        let _ = external_pc;
        let _ = local_snapshot;
        self.run_main(args)
    }
}
