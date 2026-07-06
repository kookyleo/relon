// rustc ≥ 1.93 false-positive: `unused_assignments` fires on fields of
// every `#[derive(miette::Diagnostic)]` / `thiserror::Error` enum (the
// derive expands to internal let-bindings that the lint mis-reads).
// Upstream: <https://github.com/rust-lang/rust/issues/147648>
// (stable→stable regression, P-medium, still open). Drop this `allow`
// once the rustc fix lands.
#![allow(unused_assignments)]
#![forbid(unsafe_code)]

pub mod arithmetic;
pub mod builtin_decorators;
pub mod decorator;
pub mod decorator_names;
pub mod error;
pub mod eval;
pub(crate) mod iter_protocol;
pub mod module;
pub mod native_fn;
pub mod prelude;
pub mod reference;
pub(crate) mod relon_sourced;
pub mod schema;
pub mod scope;
pub mod stdlib;
pub mod value;

// Re-export the public surface so existing callers writing `use
// relon_evaluator::Value;` (etc.) keep working after the split: every
// public type now lives in `relon-eval-api`; this crate only owns the
// tree-walking backend impl (`TreeWalkEvaluator`) and the in-tree
// stdlib / decorator registration helpers.
pub use eval::TreeWalkEvaluator;
pub use relon_eval_api::{
    Capabilities, CapabilityBit, ClosureData, Context, DecoratorPlugin, EnumSchemaData,
    EvaluatedArg, Evaluator, GatedNativeFn, ListContext, ModuleResolver, ModuleSource, NativeArgs,
    NativeFn, NativeFnCaps, NativeFnGate, PreEvalOutcome, RelonFunction, ResourceBudget,
    ResourceBudgetProfile, RootRef, RuntimeError, SchemaData, SchemaField, Scope, Thunk, Value,
    ValueDict,
};
// Concrete backend-side helpers that are not part of `relon-eval-api`.
pub use module::{FilesystemModuleResolver, StdModuleResolver};
// Phase G.W11 Phase 2: `RemoteHttpResolver` ships only when the
// `remote-http` feature is enabled (it pulls `ureq` + rustls + ring).
#[cfg(all(not(target_arch = "wasm32"), feature = "remote-http"))]
pub use module::{RemoteHttpResolver, RemoteImportCache};
pub use relon_analyzer::{MainParam, MainSignature, WorkspaceDiagnostic, WorkspaceTree};

// Tests live in dedicated files to keep the crate root focused on the
// public API surface. Each is gated by its own `#![cfg(test)]`.
#[cfg(test)]
mod eval_tests;
#[cfg(test)]
mod host_boundary_tests;
#[cfg(test)]
mod import_pin_tests;
#[cfg(test)]
mod sandbox_tests;
#[cfg(test)]
mod stdlib_drift_tests;
