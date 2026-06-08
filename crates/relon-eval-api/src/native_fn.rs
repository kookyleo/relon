//! Native function interface exposed to host code.
//!
//! Hosts implement [`RelonFunction`] and register the resulting function under
//! a path name via `Context::register_fn`. The evaluator passes a
//! [`NativeArgs`] bundle that pre-splits positional and named arguments so
//! implementations can validate either view without re-parsing.
//!
//! [`NativeFn`] is a re-export alias of `dyn RelonFunction` so call sites
//! that want a shorter spelling can write `NativeFn` instead of the longer
//! trait-object form. The two names refer to the same trait.

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
///
/// Lives in `relon-eval-api` so that any backend implementing
/// [`crate::Evaluator`] can mint a `NativeFnCaps` of its own for the native
/// fns it dispatches. The default impls keep host-supplied `RelonFunction`s
/// usable in lightweight test contexts where no backend is attached.
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
    /// `List` / `Tuple` / `Dict` element counts.
    ///
    /// The evaluator still runs a post-call `check_value_size` on
    /// every `List` / `Tuple` / `Dict` produced by a native fn (catch-all in
    /// `call_function` / `try_call_native_method`), so an intrinsic
    /// that ignores this hint is still bounded — but allocating
    /// `Vec::with_capacity(end - start)` first would OOM the host
    /// before the post-call check fires. Intrinsics that build a
    /// collection whose size is known up-front from their arguments
    /// should consult this and reject early.
    fn max_value_elements(&self) -> Option<usize> {
        None
    }

    /// Mint a fresh `Iter` cursor id under the originating Context.
    /// Used by `List.iter()` / `String.iter()` / `Dict.iter()` (and
    /// any future user-side `Iterable` constructor that wants to
    /// participate in `Iter.next()` cursor tracking) to stamp the
    /// `_id` field of the resulting `Iter`-branded dict. Returns
    /// `0` from the default impl so a non-Context-backed `caps`
    /// (e.g. in unit tests) still produces a sane id; production
    /// evaluation overrides this on the per-Context impl.
    fn next_iter_id(&self) -> u64 {
        0
    }

    /// Atomic read-check-increment of the cursor associated with
    /// `iter_id`. Returns `Some(old_cursor)` when the cursor was
    /// strictly less than `len` (and the cursor is post-incremented
    /// in the same critical section), or `None` when the cursor has
    /// reached `len` **or** the id is unknown to this Context.
    ///
    /// The "unknown id ⇒ `None`" branch is the cross-Context
    /// isolation policy: an `Iter` value built in Context A and then
    /// handed to Context B looks exhausted from B's perspective.
    /// Implementations should preserve this — silently auto-inserting
    /// a fresh cursor for an unknown id would re-introduce ambient
    /// state across Context boundaries.
    fn iter_cursor_fetch_and_inc(&self, _iter_id: u64, _len: usize) -> Option<usize> {
        None
    }

    /// Advance the step counter by `n` and bail with
    /// [`RuntimeError::StepLimitExceeded`] if the new count would exceed
    /// `max_steps`. Native fns with internal loops (`range`,
    /// `list.map / filter / reduce`, `string.split / replace`,
    /// `dict.merge`, ...) call this once per inner iteration so a
    /// million-element pipeline can't hide behind a single AST-node
    /// step.
    ///
    /// Behaviour:
    /// * `max_steps == None` → no-op (`Ok(())`), no allocation, no
    ///   lock — the default impl below mirrors that for hosts that
    ///   build a custom [`NativeFnCaps`].
    /// * `max_steps == Some(limit)` → `fetch_add(n)` on the same
    ///   atomic counter the evaluator increments; if the new value
    ///   crosses the limit, return `StepLimitExceeded { limit, range }`.
    ///
    /// `range` should pin the call-site span of the intrinsic so the
    /// resulting diagnostic points at the same node the AST-level step
    /// check would have flagged.
    fn tick(&self, _n: u64, _range: TokenRange) -> Result<(), RuntimeError> {
        Ok(())
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

/// Host-implemented native function. Registered into a [`crate::Context`]
/// via `Context::register_fn` / `register_pure_fn`.
pub trait RelonFunction: Send + Sync {
    fn call(&self, args: NativeArgs, range: TokenRange) -> Result<Value, RuntimeError>;
}

/// Convenience alias for the `dyn RelonFunction` trait object. Hosts that
/// store native fns by trait object can write `Arc<dyn NativeFn>` instead
/// of `Arc<dyn RelonFunction>` — the two are interchangeable.
pub type NativeFn = dyn RelonFunction;
