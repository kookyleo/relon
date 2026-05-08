pub mod arithmetic;
pub mod builtin_decorators;
pub mod decorator;
pub mod decorator_names;
pub mod error;
pub mod eval;
pub mod module;
pub mod native_fn;
pub mod prelude;
pub mod reference;
pub mod schema;
pub mod scope;
pub mod stdlib;
pub mod value;

pub use decorator::{DecoratorPlugin, PreEvalOutcome};
pub use error::RuntimeError;
pub use eval::{Capabilities, Context, Evaluator, NativeFnGate};
pub use module::{FilesystemModuleResolver, ModuleResolver, ModuleSource, StdModuleResolver};
pub use native_fn::NativeFnCaps;
pub use native_fn::{EvaluatedArg, NativeArgs, RelonFunction};
pub use relon_analyzer::{MainParam, MainSignature};
pub use scope::{ListContext, Scope, Thunk};
pub use value::{SchemaField, Value, ValueDict};

// Tests live in dedicated files to keep the crate root focused on the
// public API surface. Each is gated by its own `#![cfg(test)]`.
#[cfg(test)]
mod eval_tests;
#[cfg(test)]
mod host_boundary_tests;
#[cfg(test)]
mod sandbox_tests;
