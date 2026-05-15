//! Lexical and runtime context shared across evaluation steps.
//!
//! `Scope` is the single carrier of evaluator state: it walks down through the
//! AST, threads imported bindings, anchors `&root`/`&sibling` lookups, and
//! holds the lazy-evaluation thunk table. It is wrapped in `Arc` everywhere so
//! children can be derived cheaply via [`Scope::child`] and friends.

use crate::value::Value;
use relon_parser::Node;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// Iteration context for `&prev` / `&next` / `&index` references inside a list.
pub struct ListContext {
    pub index: usize,
    pub elements: Vec<Arc<Thunk>>,
}

/// Anchor for `&root` lookups in a [`Scope`].
///
/// The three fields move as a unit because they all describe one root —
/// previously they lived as three loose `Option<Arc<…>>` fields on `Scope`
/// (`reference_root` / `reference_root_scope` / `reference_root_parent`)
/// whose relationship was only documented in prose. Bundling them here
/// makes the invariant structural: `node` is always present once a root
/// is set, and the synthesized-vs-pinned distinction is encoded in
/// `scope` being `Some` or `None`.
#[derive(Clone)]
pub struct RootRef {
    /// AST node that `&root` resolves against.
    pub node: Arc<Node>,
    /// Pre-built scope already pinned at `node` — set by the dict branch
    /// of `Evaluator::eval_internal` once the dict being evaluated *is*
    /// `node`. When `None`, reference resolution synthesizes a transient
    /// root scope on demand whose parent is `parent_fallback`.
    pub scope: Option<Arc<Scope>>,
    /// Parent fallback used when synthesizing the transient root scope.
    /// Today this is the closure-body bindings scope so `&root` inside a
    /// closure body still sees the caller's bindings.
    pub parent_fallback: Option<Arc<Scope>>,
}

impl RootRef {
    /// Build a root anchor that has only the AST identity — no pinned
    /// scope, no parent fallback. The dict branch of `eval_internal`
    /// will fill `scope` later if/when it enters the matching dict.
    pub fn new(node: Arc<Node>) -> Self {
        Self {
            node,
            scope: None,
            parent_fallback: None,
        }
    }
}

/// Type alias for the bindings table. Wrapped in `Mutex` so concurrent
/// host evaluators sharing a `Scope` across threads stay safe; the
/// inner `HashMap` starts empty (zero-capacity, no heap allocation)
/// and only grows when something actually writes a binding.
///
/// Keys are stored as `Arc<str>` so that the comprehension / closure
/// hot loops can rebind the same identifier on every iteration via an
/// `Arc::clone` (refcount bump, no heap touch) instead of a full
/// `String::clone` (malloc + memcpy). `HashMap` lookup still accepts
/// `&str` queries through the standard `Borrow<str>` impl on
/// `Arc<str>`, so read sites are unchanged.
pub type Locals = Mutex<HashMap<Arc<str>, Value>>;

/// Type alias for the thunks table — same shape as [`Locals`].
pub(crate) type Thunks = Mutex<HashMap<Arc<str>, Arc<Thunk>>>;

/// Single environment frame. Cheap to derive (every field is either
/// `Clone` or `Arc`-shared) and wrapped in `Arc<Scope>` at every call
/// site so backtracking through `parent` stays copy-free.
///
/// `locals` and `thunks` are kept as `Mutex<HashMap>` rather than
/// raw `HashMap` so multiple evaluator threads can share an embedder
/// scope (e.g. the empty root scope) without external locking. The
/// inner `HashMap` is constructed empty (`HashMap::new()` does not
/// allocate until first insert), so hot-loop children that never
/// register a binding pay no heap cost for these fields beyond the
/// inline mutex word and the Scope's own `Arc` allocation.
#[derive(Default)]
pub struct Scope {
    /// Enclosing scope. `None` only at the document root.
    pub parent: Option<Arc<Scope>>,
    /// Most-recent path segment opened by [`Scope::with_path`] /
    /// [`Scope::with_list_context`]; `&sibling` / `&uncle` peel these off when
    /// rebuilding the relative target path.
    pub path_node: Option<String>,
    /// Bindings introduced inside this frame (closure params, comprehension
    /// loop vars, `where` clauses, imported aliases).
    pub locals: Locals,
    /// Working directory used when resolving relative `#import` paths.
    pub current_dir: String,
    /// Stable namespace for the path cache; usually the canonical id of the
    /// surrounding module so different modules can't collide on identical
    /// paths.
    pub cache_namespace: String,
    /// `&root` anchor. `None` only at scopes that haven't yet acquired one
    /// (typically just the pre-eval root scope before
    /// [`Evaluator::eval_root`] stamps it). See [`RootRef`] for invariants.
    pub root_ref: Option<RootRef>,
    /// Active list iteration, if any.
    pub list_context: Option<Arc<ListContext>>,
    /// Lazily-resolved bindings for the dict that owns this scope. Kept
    /// `pub(crate)` so the evaluator can register and force them, but hidden
    /// from host code.
    pub(crate) thunks: Thunks,
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("path_node", &self.path_node)
            .field("current_dir", &self.current_dir)
            .field("cache_namespace", &self.cache_namespace)
            .field("has_root_ref", &self.root_ref.is_some())
            .field("index", &self.list_context.as_ref().map(|c| c.index))
            .finish()
    }
}

impl Clone for Scope {
    fn clone(&self) -> Self {
        Self {
            parent: self.parent.clone(),
            path_node: self.path_node.clone(),
            locals: Mutex::new(self.locals.lock().unwrap().clone()),
            current_dir: self.current_dir.clone(),
            cache_namespace: self.cache_namespace.clone(),
            root_ref: self.root_ref.clone(),
            list_context: self.list_context.clone(),
            thunks: Mutex::new(self.thunks.lock().unwrap().clone()),
        }
    }
}

impl Scope {
    /// Look up `name` in this scope's locals, walking up `parent` chain.
    pub fn get_local(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.locals.lock().unwrap().get(name) {
            Some(v.clone())
        } else if let Some(parent) = &self.parent {
            parent.get_local(name)
        } else {
            None
        }
    }

    pub(crate) fn get_thunk(&self, name: &str) -> Option<Arc<Thunk>> {
        if let Some(thunk) = self.thunks.lock().unwrap().get(name) {
            Some(Arc::clone(thunk))
        } else if let Some(parent) = &self.parent {
            parent.get_thunk(name)
        } else {
            None
        }
    }

    pub(crate) fn get_own_thunk(&self, name: &str) -> Option<Arc<Thunk>> {
        self.thunks.lock().unwrap().get(name).map(Arc::clone)
    }

    /// Acquire a write lock on this scope's locals.
    ///
    /// Centralizes every write-side `scope.locals.lock().unwrap()` so
    /// the locking strategy can evolve (e.g. swap to a lock-free or
    /// thread-local representation under a feature) without touching
    /// every call site. Read paths still go through
    /// [`Scope::get_local`] which walks the parent chain.
    ///
    /// Keys are `Arc<str>`; callers writing a `String` should hand it
    /// over via `Arc::<str>::from(name)` so the buffer moves into the
    /// `Arc` allocation once, instead of paying a `String::clone` on
    /// every rebind.
    pub fn locals_for_write(&self) -> MutexGuard<'_, HashMap<Arc<str>, Value>> {
        self.locals.lock().unwrap()
    }

    /// Same as [`Scope::locals_for_write`] but for the dict thunks table.
    pub(crate) fn thunks_for_write(&self) -> MutexGuard<'_, HashMap<Arc<str>, Arc<Thunk>>> {
        self.thunks.lock().unwrap()
    }

    /// Reconstruct the path from the document root to the current scope by
    /// walking `parent` pointers and collecting `path_node` segments.
    pub fn full_path(&self) -> Vec<String> {
        let mut path = Vec::new();
        let mut current = Some(self);
        while let Some(scope) = current {
            if let Some(node) = &scope.path_node {
                path.push(node.clone());
            }
            current = scope.parent.as_deref();
        }
        path.reverse();
        path
    }

    /// Build a stable cache key for `path` under this scope's namespace.
    pub(crate) fn path_cache_key(&self, path: &[String]) -> String {
        let namespace = if self.cache_namespace.is_empty() {
            &self.current_dir
        } else {
            &self.cache_namespace
        };
        let encoded_path = path
            .iter()
            .map(|s| format!("{}:{}", s.len(), s))
            .collect::<Vec<_>>()
            .join("/");
        format!("{namespace}::{encoded_path}")
    }

    /// Open a fresh child frame inheriting flow-state fields (`current_dir`,
    /// `cache_namespace`, `root_ref`, `list_context`) from `self` but
    /// with empty `locals`/`thunks` and no `path_node`.
    ///
    /// This is the workhorse for every new lexical block — Dict body,
    /// comprehension iteration, closure body. The `with_*` methods below all
    /// build on top of it and only differ by which field they override.
    /// The empty `Mutex<HashMap>` pair doesn't touch the heap until
    /// somebody actually inserts a binding (zero-capacity HashMap +
    /// inline mutex), so the comprehension hot loop only pays for the
    /// Scope's own `Arc` allocation.
    pub fn child(self: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(Arc::clone(self)),
            path_node: None,
            locals: Mutex::new(HashMap::new()),
            current_dir: self.current_dir.clone(),
            cache_namespace: self.cache_namespace.clone(),
            root_ref: self.root_ref.clone(),
            list_context: self.list_context.clone(),
            thunks: Mutex::new(HashMap::new()),
        })
    }

    /// Bind a single `name -> val` in a fresh child frame.
    ///
    /// Accepts anything that can be cheaply turned into an `Arc<str>`
    /// (a `String`, a `&str`, or an already-shared `Arc<str>`) so hot
    /// paths can hand the key over without an extra clone. Callers
    /// that already hold an `Arc<str>` should pass `Arc::clone(&id)`
    /// to skip the heap copy entirely.
    pub fn with_local(self: &Arc<Self>, name: impl Into<Arc<str>>, val: Value) -> Arc<Self> {
        let child = self.child();
        child.locals_for_write().insert(name.into(), val);
        child
    }

    pub fn with_locals(self: &Arc<Self>, new_locals: HashMap<Arc<str>, Value>) -> Arc<Self> {
        let mut child = self.child();
        // `child()` returned an Arc with no other strong references yet,
        // so swap the freshly-built empty mutex for one already
        // seeded with `new_locals`. Skips the lock+insert round-trip
        // that a `with_local`-style write would pay on every
        // comprehension iteration.
        Arc::get_mut(&mut child)
            .expect("freshly built child has no aliases")
            .locals = Mutex::new(new_locals);
        child
    }

    pub fn with_path(self: &Arc<Self>, node: String) -> Arc<Self> {
        let mut child = self.child();
        // `child()` returns Arc with no other strong references yet, so this
        // get_mut is safe.
        Arc::get_mut(&mut child)
            .expect("freshly built child has no aliases")
            .path_node = Some(node);
        child
    }

    pub fn with_list_context(
        self: &Arc<Self>,
        index: usize,
        elements: Vec<Arc<Thunk>>,
    ) -> Arc<Self> {
        let mut child = self.child();
        let unique = Arc::get_mut(&mut child).expect("freshly built child has no aliases");
        unique.path_node = Some(index.to_string());
        unique.list_context = Some(Arc::new(ListContext { index, elements }));
        child
    }
}

/// A lazily-evaluated dict entry. The first access through
/// `Evaluator::force_thunk` parses + evaluates `node` against `scope` and
/// caches the result in `value`; later accesses return the cached value.
pub struct Thunk {
    pub(crate) node: Node,
    pub(crate) scope: Arc<Scope>,
    pub(crate) path: Vec<String>,
    pub(crate) cache_key: String,
    pub(crate) value: Mutex<Option<Value>>,
}

impl Thunk {
    pub(crate) fn new(node: Node, scope: Arc<Scope>, path: Vec<String>, cache_key: String) -> Self {
        Self {
            node,
            scope,
            path,
            cache_key,
            value: Mutex::new(None),
        }
    }
}
