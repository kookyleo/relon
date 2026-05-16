//! Native function interface re-exported from `relon-eval-api`.
//!
//! Hosts implement [`RelonFunction`] and register the resulting function
//! under a path name via `Context::register_fn`. The trait, the argument
//! bundle ([`NativeArgs`] / [`EvaluatedArg`]), and the back-call
//! capability handle ([`NativeFnCaps`]) all live in
//! `relon-eval-api::native_fn`; this module exists only as a stable
//! intra-crate import path (`use crate::native_fn::...`).

pub use relon_eval_api::native_fn::{
    EvaluatedArg, NativeArgs, NativeFn, NativeFnCaps, RelonFunction,
};
