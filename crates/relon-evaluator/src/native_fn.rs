//! Native function interface exposed to host code.
//!
//! Hosts implement [`RelonFunction`] and register the resulting function under
//! a path name via `Context::register_fn`. The evaluator passes a
//! [`NativeArgs`] bundle that pre-splits positional and named arguments so
//! implementations can validate either view without re-parsing.

use crate::error::RuntimeError;
use crate::value::Value;
use relon_parser::TokenRange;
use std::collections::HashMap;
use std::sync::Arc;

/// A single evaluated argument from a Relon call site, preserving its name if
/// it was passed as `name=value`.
#[derive(Debug, Clone)]
pub struct EvaluatedArg {
    pub name: Option<String>,
    pub value: Value,
}

impl EvaluatedArg {
    /// Construct an unnamed (positional) argument. Saves the
    /// `EvaluatedArg { name: None, value: ... }` boilerplate at call sites
    /// that synthesize implicit-self / single-arg invocations.
    pub fn positional(value: Value) -> Self {
        Self { name: None, value }
    }
}

/// A handle to the evaluator's internal execution capabilities, allowing native
/// functions to call back into Relon logic (closures).
pub trait NativeFnCaps: Send + Sync {
    fn call_relon(
        &self,
        func: &Value,
        args: Vec<Value>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError>;

    /// Expose `Capabilities::max_value_elements` to native functions
    /// so collection-building intrinsics (`range`, future bulk
    /// constructors) can pre-flight oversized requests before
    /// allocating. Returning `None` means the host imposes no cap on
    /// `List` / `Dict` element counts.
    ///
    /// The evaluator still runs a post-call `check_value_size` on
    /// every `List` / `Dict` produced by a native fn (catch-all in
    /// `call_function` / `try_call_native_method`), so an intrinsic
    /// that ignores this hint is still bounded — but allocating
    /// `Vec::with_capacity(end - start)` first would OOM the host
    /// before the post-call check fires. Intrinsics that build a
    /// collection whose size is known up-front from their arguments
    /// should consult this and reject early.
    fn max_value_elements(&self) -> Option<usize> {
        None
    }
}

/// Argument bundle handed to a [`RelonFunction`]. Positional and named
/// arguments are split apart up front so each host function only inspects
/// what it cares about.
#[derive(Clone)]
pub struct NativeArgs {
    pub positional: Vec<Value>,
    pub named: HashMap<String, Value>,
    caps: Arc<dyn NativeFnCaps>,
}

impl NativeArgs {
    pub fn new(caps: Arc<dyn NativeFnCaps>) -> Self {
        Self {
            positional: Vec::new(),
            named: HashMap::new(),
            caps,
        }
    }

    /// Split a list of evaluated args into positional + named buckets.
    pub fn from_evaluated(args: Vec<EvaluatedArg>, caps: Arc<dyn NativeFnCaps>) -> Self {
        let mut out = Self::new(caps);
        for arg in args {
            match arg.name {
                Some(name) => {
                    out.named.insert(name, arg.value);
                }
                None => out.positional.push(arg.value),
            }
        }
        out
    }

    pub fn from_positional(positional: Vec<Value>, caps: Arc<dyn NativeFnCaps>) -> Self {
        Self {
            positional,
            named: HashMap::new(),
            caps,
        }
    }

    pub fn caps(&self) -> &dyn NativeFnCaps {
        self.caps.as_ref()
    }

    /// Drop the named-argument map and yield the positional `Vec<Value>` —
    /// convenient for stdlib functions that only accept positional args.
    pub fn into_positional(self) -> Vec<Value> {
        self.positional
    }

    pub fn len(&self) -> usize {
        self.positional.len() + self.named.len()
    }

    pub fn is_empty(&self) -> bool {
        self.positional.is_empty() && self.named.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Value> {
        self.positional.get(index)
    }

    pub fn get_named(&self, name: &str) -> Option<&Value> {
        self.named.get(name)
    }
}

pub trait RelonFunction: Send + Sync {
    fn call(&self, args: NativeArgs, range: TokenRange) -> Result<Value, RuntimeError>;
}
