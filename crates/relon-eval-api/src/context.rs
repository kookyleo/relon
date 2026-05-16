//! Shared evaluator context: host policy + sandbox state.
//!
//! `Context` is the carrier of all backend-agnostic configuration: the
//! root AST node, decorator and native-fn registries, module resolvers,
//! capability grants, and the per-run caches a backend uses to thread
//! state across `eval_root` / `run_main` invocations.
//!
//! All fields are `pub` so that any backend implementing
//! [`crate::Evaluator`] in a different crate can read and update them.
//! Hosts should use the constructors and `register_*` / `with_*` helpers
//! rather than poking the fields directly.

use crate::decorator::DecoratorPlugin;
use crate::module::ModuleResolver;
use crate::native_fn::RelonFunction;
use crate::value::Value;
use relon_parser::Node;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

/// Context-wide sandbox policy. Holds the resource budgets the
/// evaluator enforces (`max_steps`, `max_value_elements`) and the
/// per-capability grant bits consulted when a host-registered native
/// function is invoked.
///
/// Per-function capability *requirements* (e.g. "this fn needs fs read")
/// live on [`NativeFnGate`]; this struct is what the host *grants*. A
/// call goes through iff every bit declared on the fn's gate is also
/// set here — there is no per-name allowlist or global short-circuit,
/// so a successful call proves that every bit on its gate was granted.
///
/// `#[non_exhaustive]`: future capability bits are added here without a
/// breaking semver bump. External callers must construct via
/// [`Capabilities::default`] / [`Capabilities::all_granted`] and mutate
/// fields, rather than struct literals.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Capabilities {
    /// Filesystem reads (host fn that calls `std::fs::read*`, also the
    /// policy bit consulted by `FilesystemModuleResolver`).
    pub reads_fs: bool,
    /// Filesystem writes (host fn that calls `std::fs::write*` /
    /// `OpenOptions::write` / `create_dir*` / `remove_*`).
    pub writes_fs: bool,
    /// Network access (sockets, HTTP clients, DNS).
    pub network: bool,
    /// Wall / monotonic clock reads (`SystemTime::now`, `Instant::now`).
    pub reads_clock: bool,
    /// Process environment reads (`std::env::var`, `args`, etc.).
    pub reads_env: bool,
    /// Random number generation (any non-deterministic source).
    pub uses_rng: bool,
    /// Maximum number of AST nodes to process before aborting.
    pub max_steps: Option<u64>,
    /// Maximum number of elements in a single List or Dict.
    pub max_value_elements: Option<usize>,
}

impl Capabilities {
    /// Audit-visible "grant everything" preset: every capability bit
    /// flipped, no step / value-size budget. The spec forbids an
    /// implicit `Context::trusted()`-style shortcut; hosts that need
    /// full grant must call this and read the resulting `Capabilities`
    /// *as data*. See `docs/zh/guide/spec.md` §4.2.
    ///
    /// Note: opening filesystem reads also requires installing a
    /// non-rejecting `FilesystemModuleResolver` (e.g.
    /// `FilesystemModuleResolver::trusted()` or
    /// `FilesystemModuleResolver::with_root_dir(...)`). The
    /// `reads_fs` flag is the policy bit; the resolver is the
    /// machinery that enforces it.
    pub fn all_granted() -> Self {
        Self {
            reads_fs: true,
            writes_fs: true,
            network: true,
            reads_clock: true,
            reads_env: true,
            uses_rng: true,
            max_steps: None,
            max_value_elements: None,
        }
    }
}

/// Capability requirements declared *per native function* at registration
/// time. The gate compares these against the context-wide
/// [`Capabilities`] grant when the function is invoked under sandbox.
///
/// A pure function (no host capability needed) carries
/// `NativeFnGate::default()` — every bit zero. The gate check is
/// trivially satisfied by any `Capabilities` value, including a
/// fully-sandboxed [`Capabilities::default`].
///
/// `#[non_exhaustive]`: future capability bits are added here without a
/// breaking semver bump. External callers should construct via
/// `NativeFnGate::default()` and set the bits they need.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NativeFnGate {
    /// Function reads from the filesystem.
    pub reads_fs: bool,
    /// Function writes to or mutates the filesystem.
    pub writes_fs: bool,
    /// Function makes network requests.
    pub network: bool,
    /// Function reads wall / monotonic clocks.
    pub reads_clock: bool,
    /// Function reads process environment.
    pub reads_env: bool,
    /// Function consumes randomness from a non-deterministic source.
    pub uses_rng: bool,
}

impl NativeFnGate {
    /// Capability bits required by this gate that are *not* granted in
    /// `caps`. Iteration order is the field-declaration order; runtime
    /// uses the first entry as the failure reason, analyzer emits one
    /// diagnostic per entry.
    pub fn missing_bits(&self, caps: &Capabilities) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.reads_fs && !caps.reads_fs {
            out.push("reads_fs");
        }
        if self.writes_fs && !caps.writes_fs {
            out.push("writes_fs");
        }
        if self.network && !caps.network {
            out.push("network");
        }
        if self.reads_clock && !caps.reads_clock {
            out.push("reads_clock");
        }
        if self.reads_env && !caps.reads_env {
            out.push("reads_env");
        }
        if self.uses_rng && !caps.uses_rng {
            out.push("uses_rng");
        }
        out
    }
}

/// Internal helper: a registered native function with its capability gate.
/// `pub` so backend crates can read both the underlying `func` and the
/// declared `gate` when dispatching a call.
pub struct GatedNativeFn {
    pub func: Arc<dyn RelonFunction>,
    pub gate: NativeFnGate,
}

/// Shared execution environment for one or more evaluations.
///
/// Holds the document root, registered plugins, cached modules, and
/// sandbox [`Capabilities`]. Thread-safe.
///
/// All fields are `pub` so any backend implementing [`crate::Evaluator`]
/// from a separate crate can read and update them. Hosts should prefer
/// the constructor / `register_*` / `with_*` helpers.
pub struct Context {
    pub root_node: Option<Arc<Node>>,
    pub decorators: HashMap<String, Arc<dyn DecoratorPlugin>>,
    pub functions: HashMap<String, GatedNativeFn>,
    /// Schema-rooted Phase D: native methods registered against a
    /// specific schema. Keyed by `(schema_name, method_name)` so a
    /// host can attach `register_method("Money", "cents_value", gate,
    /// func)` and the evaluator dispatches `m.cents_value()` to it
    /// when `m`'s brand is `"Money"`. Mirrors the analyzer's
    /// `tree.method_signatures` shape; the `#native` directive on a
    /// `with { ... }` method declares the slot, the host fills it at
    /// runtime through this map.
    pub native_methods: HashMap<(String, String), GatedNativeFn>,
    pub schemas: HashMap<String, Value>,
    pub module_resolvers: Vec<Arc<dyn ModuleResolver>>,
    pub path_cache: Mutex<HashMap<String, Value>>,
    pub module_cache: Mutex<HashMap<String, Value>>,
    /// Backing cursor table for user-callable `Iter.next()`. Keyed by
    /// the `u64` iter-id minted by [`Context::next_iter_id`] at the
    /// `iter()` call site and stamped into the resulting `Iter`-branded
    /// dict as `_id`. The `Value` graph is immutable (`Arc`-shared, no
    /// interior mutability), so cursor state must live outside it; this
    /// Context field is the canonical home — entries die when the
    /// Context is dropped, and the table is cleared at the start of
    /// every top-level `eval_root` / `run_main` so long-running hosts
    /// reusing a Context never accumulate stale cursors. Cross-Context
    /// `Iter` values surface as exhausted (`next()` returns `None`):
    /// see `NativeFnCaps::iter_cursor_fetch_and_inc`.
    pub iter_cursors: Mutex<HashMap<u64, usize>>,
    /// Monotonic per-Context id generator paired with
    /// [`Context::iter_cursors`]. Wraps at `u64::MAX`, effectively
    /// never reached in practice. Deliberately not reset on
    /// `eval_root` / `run_main` cleanup — the cursor table is, but
    /// the counter must keep climbing so a still-live `Iter` dict
    /// from the prior run can't collide with a fresh one in the
    /// new run.
    pub iter_id_counter: AtomicU64,
    /// Modules currently on the load stack, with a re-entry counter so
    /// the same canonical id can appear multiple times (e.g. via `as=`
    /// vs `spread=true`) without the inner guard's `Drop` clearing the
    /// outer frame's record. Decrement on drop, remove when zero.
    pub loading_modules: Mutex<HashMap<String, usize>>,
    pub evaluating_paths: Mutex<HashSet<String>>,
    pub step_counter: AtomicU64,
    /// Monotonic counter incremented once per closure invocation. Used
    /// by `eval_closure` to derive a fresh `cache_namespace` for each
    /// call so that path-cache entries computed inside the closure body
    /// (e.g. `&sibling.x`) are not shared across distinct invocations
    /// with different bound parameters.
    pub closure_call_counter: AtomicU64,
    pub analyzed: Option<Arc<relon_analyzer::AnalyzedTree>>,
    /// Pre-computed workspace tree (entry + every reachable module),
    /// produced by `relon_analyzer::analyze_entry`. When present, the
    /// evaluator's `evaluate_module_source` skips the per-module
    /// parse-plus-analyze pass and looks up the cached node and
    /// analyzed tree directly. The field is independent of
    /// `analyzed`; the latter remains the side-table for the entry
    /// file specifically, so existing callers that don't drive
    /// workspace analysis keep working unchanged.
    pub workspace: Option<Arc<relon_analyzer::WorkspaceTree>>,
    pub capabilities: Capabilities,
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// Construct an empty [`Context`] with no registered plugins or
    /// resolvers. Backend crates (`relon-evaluator`) seed their own
    /// stdlib / decorator / prelude on top of this. Hosts that want
    /// the "ready-to-use" preset should call the backend's own
    /// constructor (e.g. `TreeWalkEvaluator`-side helpers re-export
    /// here through the `relon` facade).
    ///
    /// This bare constructor exists so any backend can build a
    /// `Context` without pulling in the tree-walk evaluator's stdlib.
    pub fn new() -> Self {
        Self {
            root_node: None,
            decorators: HashMap::new(),
            functions: HashMap::new(),
            native_methods: HashMap::new(),
            schemas: HashMap::new(),
            module_resolvers: Vec::new(),
            path_cache: Mutex::new(HashMap::new()),
            module_cache: Mutex::new(HashMap::new()),
            iter_cursors: Mutex::new(HashMap::new()),
            iter_id_counter: AtomicU64::new(0),
            loading_modules: Mutex::new(HashMap::new()),
            evaluating_paths: Mutex::new(HashSet::new()),
            step_counter: AtomicU64::new(0),
            closure_call_counter: AtomicU64::new(0),
            analyzed: None,
            workspace: None,
            capabilities: Capabilities::default(),
        }
    }

    pub fn with_root(mut self, node: Node) -> Self {
        self.root_node = Some(Arc::new(node));
        self
    }

    pub fn with_analyzed(mut self, tree: Arc<relon_analyzer::AnalyzedTree>) -> Self {
        self.analyzed = Some(tree);
        self
    }

    /// Wire a pre-computed workspace tree into the context. The
    /// workspace's entry tree (if present) is also installed as
    /// `analyzed` so callers that read either field see consistent
    /// data — gives single-file consumers the same view they had
    /// before, and gives module-loading code a fast path to skip
    /// per-module parse + analyze.
    pub fn with_workspace(mut self, workspace: Arc<relon_analyzer::WorkspaceTree>) -> Self {
        if let Some(entry) = workspace.modules.get(&workspace.entry_id) {
            self.analyzed = Some(Arc::clone(entry));
        }
        self.workspace = Some(workspace);
        self
    }

    pub fn prepend_module_resolver(&mut self, resolver: Arc<dyn ModuleResolver>) {
        self.module_resolvers.insert(0, resolver);
    }

    /// Register a native function with explicit capability requirements.
    /// The function declares which bits it needs via `gate`; under the
    /// sandbox the call is rejected unless every set bit is granted in
    /// the context-wide [`Capabilities`].
    ///
    /// For pure functions (no host capability, no I/O, no ambient
    /// state) prefer [`Self::register_pure_fn`] — it makes the
    /// "this fn is pure" intent explicit. Passing
    /// `NativeFnGate::default()` here is equivalent.
    pub fn register_fn<S: Into<String>>(
        &mut self,
        name: S,
        gate: NativeFnGate,
        func: Arc<dyn RelonFunction>,
    ) {
        self.functions
            .insert(name.into(), GatedNativeFn { func, gate });
    }

    /// Register a pure native function: no I/O, no ambient state, no
    /// host capability required. Equivalent to
    /// `register_fn(name, NativeFnGate::default(), func)`. The all-zero
    /// gate is trivially satisfied by every `Capabilities` value, so
    /// pure fns keep working under a fully sandboxed context.
    ///
    /// Stdlib intrinsics (`len`, `range`, `string.*`, …) and
    /// deterministic host fns whose contract is "args in, value out"
    /// register through this entry point.
    pub fn register_pure_fn<S: Into<String>>(&mut self, name: S, func: Arc<dyn RelonFunction>) {
        self.register_fn(name, NativeFnGate::default(), func);
    }

    /// Schema-rooted Phase D: attach a host-supplied implementation to
    /// a `#native` method on a specific schema. The evaluator
    /// dispatches `value.method(...)` to this fn whenever `value`'s
    /// brand matches `schema` and the source-side method body is
    /// absent (declared `#native`). Capability gating mirrors
    /// [`Self::register_fn`]: the `gate` declares which
    /// [`Capabilities`] bits the body needs at runtime, and a denied
    /// caller surfaces `RuntimeError::CapabilityDenied`.
    ///
    /// Replaces the v1 pattern of `register_fn("Schema.method", ...)`
    /// with a key shape that tracks the schema-rooted dispatch model
    /// directly — no string concatenation, no shadowing of free fn
    /// names by accident.
    pub fn register_method<S: Into<String>, M: Into<String>>(
        &mut self,
        schema: S,
        method: M,
        gate: NativeFnGate,
        func: Arc<dyn RelonFunction>,
    ) {
        self.native_methods
            .insert((schema.into(), method.into()), GatedNativeFn { func, gate });
    }

    /// Pure-method counterpart to [`Self::register_method`]. Equivalent
    /// to passing [`NativeFnGate::default`] (the all-zero gate) — the
    /// method body needs no host capability, so it dispatches under
    /// every [`Capabilities`] including the zero-trust default.
    pub fn register_pure_method<S: Into<String>, M: Into<String>>(
        &mut self,
        schema: S,
        method: M,
        func: Arc<dyn RelonFunction>,
    ) {
        self.register_method(schema, method, NativeFnGate::default(), func);
    }

    pub fn register_decorator<S: Into<String>>(
        &mut self,
        name: S,
        plugin: Arc<dyn DecoratorPlugin>,
    ) {
        self.decorators.insert(name.into(), plugin);
    }

    pub fn register_schema<S: Into<String>>(&mut self, name: S, schema: Value) {
        self.schemas.insert(name.into(), schema);
    }

    pub fn enter_loading_module(&self, id: String) -> LoadingModuleGuard<'_> {
        *self
            .loading_modules
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_insert(0) += 1;
        LoadingModuleGuard {
            context: self,
            module_id: id,
        }
    }

    pub fn analyzer_target(&self, id: relon_parser::NodeId) -> Option<Node> {
        self.analyzed
            .as_ref()
            .and_then(|tree| tree.node(id).map(|arc| (**arc).clone()))
    }

    /// Mint a fresh `Iter` cursor id under this Context **and seed a
    /// zero cursor entry** so that subsequent
    /// [`Context::iter_cursor_fetch_and_inc`] calls can distinguish a
    /// "freshly minted, cursor at 0" iter from a foreign-Context iter
    /// (no entry → treated as exhausted; see policy note on
    /// `iter_cursor_fetch_and_inc`).
    ///
    /// Each `xs.iter()` consumes one id; two Contexts mint
    /// independently because each owns its own counter. Wraps at
    /// `u64::MAX` — reachable only in pathological constructions —
    /// and the `Relaxed` ordering is sufficient because the id is
    /// opaque outside of [`Context::iter_cursors`] lookup.
    pub fn next_iter_id(&self) -> u64 {
        use std::sync::atomic::Ordering;
        let id = self.iter_id_counter.fetch_add(1, Ordering::Relaxed);
        // Pre-register the cursor so the "missing entry → exhausted"
        // signal in `iter_cursor_fetch_and_inc` cleanly distinguishes
        // a foreign-Context `_id` from a fresh local one.
        self.iter_cursors.lock().unwrap().insert(id, 0);
        id
    }

    /// Atomically read the cursor for `iter_id`, and if `cursor < len`,
    /// post-increment and return the old value; otherwise return
    /// `None`. **A missing entry** (no cursor was ever minted for
    /// `iter_id` in this Context) is also reported as `None` —
    /// idempotent end-of-iter, matching the `Option::None` return
    /// type of `Iter.next() -> Option<T>`.
    ///
    /// Cross-Context policy (deliberate): if the host hands an
    /// `Iter` value built in Context A to Context B and then calls
    /// `next()`, Context B's table has no entry for that id, so we
    /// return `None`. This is the gentlest reading of "an iter
    /// belongs to its originating Context" — no new error variant,
    /// no capability trap; the iter simply looks exhausted to the
    /// foreign Context. A future stricter mode could surface a
    /// dedicated `RuntimeError::IterNotOwnedByContext`, but today's
    /// host APIs don't yet expose a way to attach an iter to a
    /// Context other than via `iter()` itself, so the implicit-
    /// exhausted reading is sufficient and matches the
    /// "no implicit ambient state" design promise.
    pub fn iter_cursor_fetch_and_inc(&self, iter_id: u64, len: usize) -> Option<usize> {
        // Single-lock atomic read-check-increment. Spelled out so
        // the bounds check and the bump happen under the same
        // critical section — splitting them would let a concurrent
        // caller observe a stale "in bounds" reading after the
        // cursor moved.
        let mut cursors = self.iter_cursors.lock().unwrap();
        // Do *not* `entry(...).or_insert(0)`: a foreign-Context id
        // must surface as `None` rather than silently spawn a fresh
        // cursor in this Context's table (which would start it
        // walking from 0 against a `_source` the caller's Context
        // never validated).
        let cursor_slot = cursors.get_mut(&iter_id)?;
        if *cursor_slot < len {
            let idx = *cursor_slot;
            *cursor_slot += 1;
            Some(idx)
        } else {
            None
        }
    }
}

pub struct LoadingModuleGuard<'a> {
    context: &'a Context,
    module_id: String,
}

impl Drop for LoadingModuleGuard<'_> {
    fn drop(&mut self) {
        let mut loading = self.context.loading_modules.lock().unwrap();
        if let Some(count) = loading.get_mut(&self.module_id) {
            *count -= 1;
            if *count == 0 {
                loading.remove(&self.module_id);
            }
        }
    }
}
