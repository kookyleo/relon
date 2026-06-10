//! Native-fn shims whose behaviour comes from the bundled
//! `std_relon/*.relon` source instead of a Rust body.
//!
//! Several stdlib functions used to exist twice: once as a native Rust
//! `RelonFunction` (e.g. `MathMin`) and once as a relon-side wrapper in
//! `std_relon/math.relon` that merely delegated back to the native via an
//! underscore intrinsic. That made the `.relon` file look like the source
//! of truth while the actual semantics lived in Rust — two places to keep
//! in sync, and drift between them is exactly the class of bug the
//! four-way differential suite exists to catch.
//!
//! [`RelonSourcedFn`] removes the twin: the registered name (bare `min`,
//! underscore `_math_min`, the `List` method `contains`, ...) dispatches to
//! the *relon implementation* parsed out of the bundled module source. The
//! `.relon` text is the single source of truth; the Rust side keeps only
//! the registration plumbing.
//!
//! Mechanics: on first use the bundled module source is parsed and
//! evaluated once per process (the body is a dict of function definitions,
//! so evaluation just mints closures — nothing user-visible runs). The
//! resulting member closure is self-contained (`ClosureData` carries its
//! params / body / captured env), so the shim hands it to the *caller's*
//! evaluator via [`NativeFnCaps::call_relon`] — step budget, recursion
//! limits and intrinsic lookups (`_math_abs`, `_list_reduce`, `type`) all
//! resolve against the calling `Context`, exactly as if the user had
//! imported the module and called the member directly.

use crate::error::RuntimeError;
use crate::native_fn::{NativeArgs, RelonFunction};
use crate::value::Value;
use relon_eval_api::context::Context;
use relon_eval_api::scope::Scope;
use relon_parser::TokenRange;
use std::sync::{Arc, OnceLock};

/// Bundled std modules a [`RelonSourcedFn`] can draw members from.
#[derive(Clone, Copy)]
pub(crate) enum StdModule {
    Math,
    List,
}

impl StdModule {
    fn path(self) -> &'static str {
        match self {
            StdModule::Math => "std/math",
            StdModule::List => "std/list",
        }
    }

    /// Same embedded bytes `StdModuleResolver` serves for `#import`, so
    /// the shim and the import path can never disagree on the source.
    fn source(self) -> &'static str {
        match self {
            StdModule::Math => include_str!("std_relon/math.relon"),
            StdModule::List => include_str!("std_relon/list.relon"),
        }
    }

    /// Process-level cache of the evaluated module dict. Stored as
    /// `Result<_, String>` (not `RuntimeError`) so a hypothetical failure
    /// is reproducible on every call instead of being observed once and
    /// lost; with the bundled sources covered by the stdlib drift tests
    /// the `Err` arm is unreachable in practice.
    fn evaluated(self) -> &'static Result<Value, String> {
        static MATH: OnceLock<Result<Value, String>> = OnceLock::new();
        static LIST: OnceLock<Result<Value, String>> = OnceLock::new();
        let cell = match self {
            StdModule::Math => &MATH,
            StdModule::List => &LIST,
        };
        cell.get_or_init(|| eval_bundled_module(self.path(), self.source()))
    }
}

/// Evaluate a bundled module source into its dict-of-closures value.
///
/// The throwaway `Context` only hosts this one evaluation; the closures it
/// mints are self-contained, and every later *call* runs on the caller's
/// own evaluator (see module docs). The module body is a dict literal of
/// function definitions, so this walk allocates a handful of closures and
/// never executes user-observable logic.
fn eval_bundled_module(path: &'static str, source: &'static str) -> Result<Value, String> {
    let node = relon_parser::parse_document(source)
        .map_err(|error| format!("parse error in bundled `{path}`: {error:?}"))?;
    let mut ctx = Context::new().with_root(node);
    crate::eval::TreeWalkEvaluator::prepare_in_place(&mut ctx);
    crate::eval::TreeWalkEvaluator::new(Arc::new(ctx))
        .eval_root(&Arc::new(Scope::default()))
        .map_err(|error| format!("eval error in bundled `{path}`: {error}"))
}

/// A registered stdlib function whose implementation is the relon closure
/// `<module>.<member>` from the bundled `std_relon` source.
pub(crate) struct RelonSourcedFn {
    module: StdModule,
    member: &'static str,
}

impl RelonSourcedFn {
    pub(crate) fn new(module: StdModule, member: &'static str) -> Self {
        Self { module, member }
    }

    fn member_closure(&self, range: TokenRange) -> Result<Value, RuntimeError> {
        let value = self
            .module
            .evaluated()
            .as_ref()
            .map_err(|message| RuntimeError::ModuleParseError {
                path: self.module.path().to_string(),
                message: message.clone(),
                range: range.into(),
            })?;
        let member = match value {
            Value::Dict(dict) => dict.map.get(self.member),
            _ => None,
        };
        member.cloned().ok_or_else(|| {
            RuntimeError::FunctionNotFound(
                format!("{}.{}", self.module.path(), self.member),
                range,
            )
        })
    }
}

impl RelonFunction for RelonSourcedFn {
    fn call(&self, mut args: NativeArgs, range: TokenRange) -> Result<Value, RuntimeError> {
        let func = self.member_closure(range)?;
        let positional = std::mem::take(&mut args.positional);
        args.caps().call_relon(&func, positional, range)
    }
}
