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

/// Argument bundle handed to a [`RelonFunction`]. Positional and named
/// arguments are split apart up front so each host function only inspects
/// what it cares about.
#[derive(Debug, Default, Clone)]
pub struct NativeArgs {
    pub positional: Vec<Value>,
    pub named: HashMap<String, Value>,
}

impl NativeArgs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Split a list of evaluated args into positional + named buckets.
    pub fn from_evaluated(args: Vec<EvaluatedArg>) -> Self {
        let mut out = Self::default();
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

    pub fn from_positional(positional: Vec<Value>) -> Self {
        Self {
            positional,
            named: HashMap::new(),
        }
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
