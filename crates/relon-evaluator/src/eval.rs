use crate::decorator::{DecoratorPlugin, PreEvalOutcome};
use crate::error::RuntimeError;
use crate::module::{FilesystemModuleResolver, ModuleResolver, ModuleSource, StdModuleResolver};
use crate::native_fn::{EvaluatedArg, NativeArgs, NativeFnCaps, RelonFunction};
use crate::scope::{Scope, Thunk};
use crate::value::Value;
use relon_parser::{
    is_builtin_type_name, parse_document, CallArg, Decorator as DecoratorNode, Expr, FStringPart,
    Node, Operator, TokenKey, TokenRange,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Context-wide sandbox policy. Holds both the resource budgets the
/// evaluator enforces (`max_steps`, `max_value_bytes`) and the
/// allow-lists used to gate calls to host-registered native functions.
///
/// Per-function capability *requirements* (e.g. "this fn needs fs read")
/// live on [`NativeFnGate`]; this struct is what the host *grants*.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// If true, all registered native functions can be called.
    pub allow_all_native_fn: bool,
    /// Set of specifically allowed native function names (e.g. `["math.sum"]`).
    pub allow_native_fn: HashSet<String>,
    /// If true, filesystem-based module resolution is permitted.
    pub reads_fs: bool,
    /// Maximum number of AST nodes to process before aborting.
    pub max_steps: Option<u64>,
    /// Maximum number of elements in a single List or Dict.
    pub max_value_bytes: Option<usize>,
}

impl Capabilities {
    /// Audit-visible "grant everything" preset: all native functions
    /// allowed, filesystem reads permitted, no step / value-size
    /// budget. The spec forbids an implicit `Context::trusted()`-style
    /// shortcut; hosts that need full grant must call this and read
    /// the resulting `Capabilities` *as data*. See `docs/zh/guide/spec.md`
    /// §4.2.
    ///
    /// Note: opening filesystem reads also requires installing a
    /// non-rejecting [`crate::module::FilesystemModuleResolver`] (e.g.
    /// `FilesystemModuleResolver::trusted()` or
    /// `FilesystemModuleResolver::with_root_dir(...)`). The
    /// `reads_fs` flag is the policy bit; the resolver is the
    /// machinery that enforces it.
    pub fn all_granted() -> Self {
        Self {
            allow_all_native_fn: true,
            allow_native_fn: HashSet::new(),
            reads_fs: true,
            max_steps: None,
            max_value_bytes: None,
        }
    }
}

/// Capability requirements declared *per native function* at registration
/// time. The gate compares these against the context-wide
/// [`Capabilities`] grant when the function is invoked under sandbox.
///
/// Kept distinct from `Capabilities` so the per-fn record can grow
/// independently (future: `network`, `env`, `writes_fs`, …) without
/// dragging context-only fields like `max_steps` into per-fn metadata.
#[derive(Debug, Clone, Default)]
pub struct NativeFnGate {
    /// The function reads from the filesystem (callers must hold
    /// `Capabilities::reads_fs` to invoke it under sandbox).
    pub reads_fs: bool,
}

pub(crate) struct GatedNativeFn {
    pub(crate) func: Arc<dyn RelonFunction>,
    pub(crate) gated: bool,
    pub(crate) gate: NativeFnGate,
}

/// Shared execution environment for one or more evaluations.
///
/// Holds the document root, registered plugins, cached modules, and
/// sandbox [`Capabilities`]. Thread-safe.
pub struct Context {
    pub(crate) root_node: Option<Arc<Node>>,
    pub(crate) decorators: HashMap<String, Arc<dyn DecoratorPlugin>>,
    pub(crate) functions: HashMap<String, GatedNativeFn>,
    pub schemas: HashMap<String, Value>,
    pub(crate) module_resolvers: Vec<Arc<dyn ModuleResolver>>,
    pub(crate) path_cache: Mutex<HashMap<String, Value>>,
    pub(crate) module_cache: Mutex<HashMap<String, Value>>,
    /// Modules currently on the load stack, with a re-entry counter so
    /// the same canonical id can appear multiple times (e.g. via `as=`
    /// vs `spread=true`) without the inner guard's `Drop` clearing the
    /// outer frame's record. Decrement on drop, remove when zero.
    pub(crate) loading_modules: Mutex<HashMap<String, usize>>,
    pub(crate) evaluating_paths: Mutex<HashSet<String>>,
    pub(crate) step_counter: AtomicU64,
    /// Monotonic counter incremented once per closure invocation. Used
    /// by `eval_closure` to derive a fresh `cache_namespace` for each
    /// call so that path-cache entries computed inside the closure body
    /// (e.g. `&sibling.x`) are not shared across distinct invocations
    /// with different bound parameters.
    pub(crate) closure_call_counter: AtomicU64,
    pub analyzed: Option<Arc<relon_analyzer::AnalyzedTree>>,
    pub capabilities: Capabilities,
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// Minimal context: virtual `std/...` resolver, builtin decorators,
    /// and the pure-functional stdlib (`len`, `range`, `string.*`,
    /// `math.*`, …) registered via [`Self::register_fn`] (i.e. ungated —
    /// they're considered trusted infrastructure regardless of sandbox
    /// state). No filesystem resolver is mounted; `@import("./x.relon")`
    /// will fall through to a `ModuleNotFound`. Use [`Self::sandboxed`]
    /// for real workloads and then grant capabilities explicitly.
    pub fn new() -> Self {
        let mut this = Self {
            root_node: None,
            decorators: HashMap::new(),
            functions: HashMap::new(),
            schemas: HashMap::new(),
            module_resolvers: Vec::new(),
            path_cache: Mutex::new(HashMap::new()),
            module_cache: Mutex::new(HashMap::new()),
            loading_modules: Mutex::new(HashMap::new()),
            evaluating_paths: Mutex::new(HashSet::new()),
            step_counter: AtomicU64::new(0),
            closure_call_counter: AtomicU64::new(0),
            analyzed: None,
            capabilities: Capabilities::default(),
        };
        crate::builtin_decorators::register_to(&mut this);
        crate::stdlib::register_to(&mut this);
        // Virtual Stdlib is checked first
        this.module_resolvers.push(Arc::new(StdModuleResolver));
        this
    }

    /// Sandboxed context for untrusted scripts. Adds a default-rejecting
    /// [`FilesystemModuleResolver`] after the virtual `std/...` resolver
    /// so `@import("std/list")` works while `@import("./local.relon")`
    /// returns `CapabilityDenied`. `Capabilities` defaults are
    /// restrictive: no fs grant, no native-fn allowlist.
    ///
    /// **Sandbox scope:** this only constrains filesystem `#import` and
    /// host-registered functions added via [`Self::register_fn_with_caps`].
    /// The pure-functional stdlib (registered via [`Self::register_fn`])
    /// is intentionally ungated — those functions perform no I/O and
    /// have no side-effects beyond their return value. Hosts that need
    /// to forbid even the stdlib should re-register the relevant entries
    /// via `register_fn_with_caps` after construction.
    pub fn sandboxed() -> Self {
        let mut this = Self::new();
        this.module_resolvers
            .push(Arc::new(FilesystemModuleResolver::default()));
        this
    }

    pub fn with_root(mut self, node: Node) -> Self {
        self.root_node = Some(Arc::new(node));
        self
    }

    pub fn with_analyzed(mut self, tree: Arc<relon_analyzer::AnalyzedTree>) -> Self {
        self.analyzed = Some(tree);
        self
    }

    pub fn prepend_module_resolver(&mut self, resolver: Arc<dyn ModuleResolver>) {
        self.module_resolvers.insert(0, resolver);
    }

    /// Register a fully-trusted native function. Calls bypass the sandbox
    /// gate entirely — equivalent to "all caps true". Use
    /// [`Self::register_fn_with_caps`] for anything that needs to be
    /// guarded by host policy.
    pub fn register_fn<S: Into<String>>(&mut self, name: S, func: Arc<dyn RelonFunction>) {
        self.functions.insert(
            name.into(),
            GatedNativeFn {
                func,
                gated: false,
                gate: NativeFnGate::default(),
            },
        );
    }

    /// Register a native function whose calls are gated by the
    /// context-wide [`Capabilities`]. The function declares what it
    /// *requires* via [`NativeFnGate`] (e.g. `reads_fs: true`); under
    /// sandbox the call is rejected unless either
    /// `Capabilities::allow_all_native_fn` is on or `name` appears in
    /// `Capabilities::allow_native_fn`.
    pub fn register_fn_with_caps<S: Into<String>>(
        &mut self,
        name: S,
        gate: NativeFnGate,
        func: Arc<dyn RelonFunction>,
    ) {
        self.functions.insert(
            name.into(),
            GatedNativeFn {
                func,
                gated: true,
                gate,
            },
        );
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

pub struct Evaluator {
    pub context: Arc<Context>,
    /// Lazy cache for the `Arc<dyn NativeFnCaps>` handed to native fns so
    /// closures can call back into Relon. Allocating one per call shows up
    /// in the per-element hot path of `_list_map`/`_list_filter` etc.
    caps: std::sync::OnceLock<Arc<EvaluatorCaps>>,
    /// Cached empty scope used as the parent of native-fn closure
    /// callbacks. Avoids one `Arc::new(Scope::default())` per element.
    empty_scope: std::sync::OnceLock<Arc<Scope>>,
}

impl Evaluator {
    pub fn new(context: Arc<Context>) -> Self {
        Self {
            context,
            caps: std::sync::OnceLock::new(),
            empty_scope: std::sync::OnceLock::new(),
        }
    }

    fn caps(&self) -> Arc<dyn NativeFnCaps> {
        self.caps
            .get_or_init(|| {
                Arc::new(EvaluatorCaps {
                    context: Arc::clone(&self.context),
                })
            })
            .clone()
    }

    fn empty_scope(&self) -> &Arc<Scope> {
        self.empty_scope.get_or_init(|| Arc::new(Scope::default()))
    }

    fn is_valid_identifier(s: &str) -> bool {
        if s.is_empty() {
            return false;
        }
        let mut chars = s.chars();
        let first = chars.next().unwrap();
        if !first.is_ascii_alphabetic() && first != '_' {
            return false;
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    }

    fn is_logic_definition(node: &Node) -> bool {
        matches!(node.expr.as_ref(), Expr::Closure { .. })
    }

    pub fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        self.eval_internal(node, scope, false)
    }

    /// Enforce `Capabilities::max_value_bytes`. The field name is for
    /// forward compatibility with a future byte-accurate metric; today
    /// we measure element count for `List` / `Dict` and skip primitive
    /// values entirely (their size is bounded by the source).
    pub(crate) fn check_value_size(
        &self,
        value: &Value,
        range: TokenRange,
    ) -> Result<(), RuntimeError> {
        let Some(limit) = self.context.capabilities.max_value_bytes else {
            return Ok(());
        };
        let actual = match value {
            Value::List(l) => l.len(),
            Value::Dict(d) => d.map.len(),
            _ => return Ok(()),
        };
        if actual > limit {
            Err(RuntimeError::ValueTooLarge {
                limit,
                actual,
                range,
            })
        } else {
            Ok(())
        }
    }

    /// Evaluate the document attached to `Context::with_root` as a
    /// **library / static config** — i.e. without consulting any
    /// `#main(...)` signature or pushing host args. Stamps
    /// `scope.root_ref` with the root node so `&root` references resolve
    /// correctly during the walk; the existence check against an existing
    /// `root_ref` lets nested modules preserve their own root binding when
    /// re-entering this from `load_module`.
    ///
    /// For files that declare `#main(...)` use [`Self::run_main`]
    /// instead — it validates and binds the host-pushed args before the
    /// body walk.
    pub fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        // Reset the step budget so hosts that reuse one `Evaluator` for
        // multiple independent top-level evaluations don't carry over
        // counts from prior runs. Module loads happen *inside* this
        // top-level walk and intentionally do not reset.
        self.context.step_counter.store(0, Ordering::Relaxed);
        let root = self.context.root_node.clone().ok_or_else(|| {
            RuntimeError::VariableNotFound(
                "Context has no root node — call `Context::with_root` first".to_string(),
                TokenRange::default(),
            )
        })?;
        let scope = self.prepare_root_scope(scope, &root)?;
        self.eval(&root, &scope)
    }

    /// Evaluate the document as an entry program: validate `args`
    /// against the file's `#main(...)` signature (each declared
    /// parameter must appear with a value of the declared type), bind
    /// every parameter into the root scope's locals, then evaluate the
    /// body.
    ///
    /// Errors:
    /// * [`RuntimeError::NoMainSignature`] — the file lacks a
    ///   `#main(...)` directive.
    /// * [`RuntimeError::MissingMainArg`] — host didn't push a value for
    ///   a declared parameter.
    /// * [`RuntimeError::UnexpectedMainArg`] — host pushed an arg name
    ///   not in the signature.
    /// * [`RuntimeError::MainArgTypeMismatch`] — pushed value doesn't
    ///   match the parameter's declared type.
    pub fn run_main(
        &self,
        scope: &Arc<Scope>,
        mut args: HashMap<String, Value>,
    ) -> Result<Value, RuntimeError> {
        // Reset the step budget — see `eval_root` for rationale.
        self.context.step_counter.store(0, Ordering::Relaxed);
        let root = self.context.root_node.clone().ok_or_else(|| {
            RuntimeError::VariableNotFound(
                "Context has no root node — call `Context::with_root` first".to_string(),
                TokenRange::default(),
            )
        })?;
        let signature = self
            .context
            .analyzed
            .as_ref()
            .and_then(|tree| tree.main_signature.clone())
            .ok_or_else(|| RuntimeError::NoMainSignature { range: root.range })?;
        let scope = self.prepare_root_scope(scope, &root)?;

        // Schema scope: root-level `#schema A Body` declarations must be
        // visible before we type-check arguments referring to them, and
        // dict-field `#schema X: {...}` schemas likewise. Both seedings
        // are idempotent so doing them once up-front (instead of per
        // param) is enough.
        if let Some(tree) = self.context.analyzed.as_ref() {
            if !tree.root_schemas.is_empty() {
                self.seed_root_schemas(&tree.root_schemas, &scope)?;
            }
        }
        self.prepare_dict_scope(&root, &scope)?;

        // Each parameter: pop the matching arg, type-check, bind into
        // scope locals. Keep a "consumed" set so we can detect extras.
        for param in &signature.params {
            let Some(mut value) = args.remove(&param.name) else {
                return Err(RuntimeError::MissingMainArg {
                    name: param.name.clone(),
                    range: param.range,
                });
            };
            self.check_type(&mut value, &param.type_node, &scope, param.range)
                .map_err(|err| match err {
                    RuntimeError::TypeMismatch {
                        expected, found, ..
                    } => RuntimeError::MainArgTypeMismatch {
                        name: param.name.clone(),
                        expected,
                        found,
                        range: param.range,
                    },
                    other => other,
                })?;
            scope
                .locals
                .lock()
                .unwrap()
                .insert(param.name.clone(), value);
        }
        if let Some((extra_name, _)) = args.into_iter().next() {
            return Err(RuntimeError::UnexpectedMainArg {
                name: extra_name,
                range: signature.range,
            });
        }

        let mut result = self.eval(&root, &scope)?;
        if let Some(return_type) = &signature.return_type {
            self.check_type(&mut result, return_type, &scope, signature.range)
                .map_err(|err| match err {
                    RuntimeError::TypeMismatch {
                        expected, found, ..
                    } => RuntimeError::MainReturnTypeMismatch {
                        expected,
                        found,
                        range: signature.range,
                    },
                    other => other,
                })?;
        }
        Ok(result)
    }

    /// Construct the root scope used by both `eval_root` and `run_main`,
    /// stamping `scope.root_ref` if needed. Both entry points share this
    /// step because the only thing that varies is whether main args
    /// flow into `scope.locals`.
    fn prepare_root_scope(
        &self,
        scope: &Arc<Scope>,
        root: &Arc<Node>,
    ) -> Result<Arc<Scope>, RuntimeError> {
        let scope = if scope.root_ref.is_none() {
            let mut overlay = (**scope).clone();
            overlay.root_ref = Some(crate::scope::RootRef::new(Arc::clone(root)));
            Arc::new(overlay)
        } else {
            Arc::clone(scope)
        };
        Ok(scope)
    }

    /// Seed every root-level `#schema X Body` declaration into
    /// `scope.locals` as a `Value::Schema`. Mirrors the dict-field
    /// `#schema X: {...}` path's runtime behavior so a `Name { ... }`
    /// reference inside the dict body — or a `#main(u: Name)` parameter
    /// type — resolves identically through the scope chain.
    ///
    /// The body node carried by each `RootSchemaDecl` is a plain dict
    /// literal (or `Enum<...>` type) with no `#schema` decorator of its
    /// own; we lower it on demand using the same pure-fn the analyzer
    /// uses (`relon_analyzer::lower_schema_pure`) and then build the
    /// runtime `Value::Schema` via `build_schema_from_def`. This keeps
    /// the field-form and decorator-form on a single lowering path.
    fn seed_root_schemas(
        &self,
        decls: &[relon_analyzer::RootSchemaDecl],
        scope: &Arc<Scope>,
    ) -> Result<(), RuntimeError> {
        for decl in decls {
            // Pre-bind the name to an empty placeholder so a recursive
            // schema body (`@schema(Tree={ children: List<Tree> })`)
            // can resolve the in-flight name during predicate building.
            // Same trick `prepare_dict_scope` uses for the field-form.
            scope.locals.lock().unwrap().insert(
                decl.name.clone(),
                Value::Schema {
                    generics: Vec::new(),
                    fields: HashMap::new(),
                },
            );
            let (lowered, _diags) = relon_analyzer::lower_schema_pure(
                Some(decl.name.clone()),
                Vec::new(),
                decl.schema_node.as_ref(),
            );
            let Some(def) = lowered else {
                // Analyzer pass already emitted the structural error;
                // bail with a runtime mirror so the host gets a
                // consistent shape.
                return Err(RuntimeError::TypeMismatch {
                    expected: "schema body (Dict or Enum<...>)".to_string(),
                    found: decl.schema_node.expr.kind().to_string(),
                    range: decl.directive_range,
                });
            };
            let value = if !def.variants.is_empty() {
                self.build_root_enum_schema(&def)
            } else {
                let fields = self.build_schema_from_def(&def, scope)?;
                Value::Schema {
                    generics: def.generics.clone(),
                    fields,
                }
            };
            scope
                .locals
                .lock()
                .unwrap()
                .insert(decl.name.clone(), value);
        }
        Ok(())
    }

    /// Build a `Value::EnumSchema` from a sum-type `SchemaDef`.
    fn build_root_enum_schema(&self, def: &relon_analyzer::SchemaDef) -> Value {
        use crate::value::SchemaField;
        let mut variants: HashMap<String, HashMap<String, SchemaField>> = HashMap::new();
        for variant in &def.variants {
            let mut fields: HashMap<String, SchemaField> = HashMap::new();
            for f in &variant.fields {
                let type_node = f
                    .type_hint
                    .clone()
                    .unwrap_or_else(|| relon_parser::TypeNode {
                        path: vec!["Any".into()],
                        generics: Vec::new(),
                        is_optional: false,
                        range: f.value_range,
                        variant_fields: None,
                        doc_comment: None,
                    });
                fields.insert(
                    f.name.clone(),
                    SchemaField {
                        type_hint: type_node,
                        predicates: vec![Value::Wildcard],
                        custom_error: None,
                        default_value: None,
                    },
                );
            }
            variants.insert(variant.name.clone(), fields);
        }
        Value::EnumSchema {
            name: def.name.clone().unwrap_or_default(),
            variants,
        }
    }

    pub(crate) fn eval_internal(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
        is_schema_pred: bool,
    ) -> Result<Value, RuntimeError> {
        if let Some(limit) = self.context.capabilities.max_steps {
            let prev = self.context.step_counter.fetch_add(1, Ordering::Relaxed);
            if prev >= limit {
                return Err(RuntimeError::StepLimitExceeded {
                    limit,
                    range: node.range,
                });
            }
        }

        let mut current_scope = Arc::clone(scope);

        // Directives in source order: `#import` rescopes; `#schema A B`
        // seeds bindings into the current scope so the body can reference
        // them before evaluation. Bare `#schema` on a dict-field overrides
        // evaluation: the body is interpreted as a schema definition
        // rather than ordinary data. Other directives are no-ops here
        // and either land elsewhere (`#main`, `#default`, `#expect`, ...)
        // or run as a post-eval transform (`#brand X` on a value).
        for dir in &node.directives {
            if let Some(override_val) = self.apply_directive_pre(dir, node, &mut current_scope)? {
                return Ok(override_val);
            }
        }

        for dec in &node.decorators {
            let name = decorator_name(dec);
            let Some(plugin) = self.context.decorators.get(&name).cloned() else {
                continue;
            };
            match plugin.pre_eval(self, node, &current_scope, &dec.args, dec.range)? {
                PreEvalOutcome::Pass => {}
                PreEvalOutcome::Rescope(new_scope) => current_scope = new_scope,
                PreEvalOutcome::Override(value) => return Ok(*value),
            }
        }

        let mut val = match node.expr.as_ref() {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::String(s) => Ok(Value::String(s.clone())),

            Expr::List(elements) => {
                let mut thunks = Vec::new();
                for (i, el) in elements.iter().enumerate() {
                    let item_scope = current_scope.with_path(i.to_string());
                    thunks.push(Arc::new(Thunk::new(
                        el.clone(),
                        item_scope,
                        Vec::new(),
                        String::new(),
                    )));
                }

                let mut values = Vec::new();
                for (i, thunk) in thunks.iter().enumerate() {
                    let item_scope = current_scope.with_list_context(i, thunks.clone());
                    let element_val = self.force_thunk_with_scope(thunk, &item_scope)?;

                    if let Expr::Spread(_) = thunk.node.expr.as_ref() {
                        if let Value::List(l) = element_val {
                            values.extend(l.iter().cloned());
                        } else {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "List".to_string(),
                                found: element_val.type_name().to_string(),
                                range: thunk.node.range,
                            });
                        }
                    } else {
                        values.push(element_val);
                    }
                }
                let result = Value::list(values);
                self.check_value_size(&result, node.range)?;
                Ok(result)
            }

            Expr::Dict(pairs) => {
                let is_root = current_scope
                    .root_ref
                    .as_ref()
                    .is_some_and(|r| std::ptr::eq(r.node.as_ref() as *const _, node as *const _));

                let mut dict_scope = Arc::new(Scope {
                    parent: Some(Arc::clone(&current_scope)),
                    path_node: None,
                    locals: Mutex::new(HashMap::new()),
                    current_dir: current_scope.current_dir.clone(),
                    cache_namespace: current_scope.cache_namespace.clone(),
                    root_ref: current_scope.root_ref.clone(),
                    list_context: current_scope.list_context.clone(),
                    thunks: Mutex::new(HashMap::new()),
                });

                if is_root {
                    let mut modified = (*dict_scope).clone();
                    if let Some(rr) = modified.root_ref.as_mut() {
                        rr.scope = Some(dict_scope.clone());
                    }
                    dict_scope = Arc::new(modified);
                }

                self.prepare_dict_scope(node, &dict_scope)?;

                let mut map = BTreeMap::new();
                for (key, value_node) in pairs {
                    match key {
                        TokenKey::Spread(_) => {
                            let val = self.eval(value_node, &dict_scope)?;
                            if let Value::Dict(d) = val {
                                for (k, v) in d.map.iter() {
                                    map.insert(k.clone(), v.clone());
                                    dict_scope
                                        .locals
                                        .lock()
                                        .unwrap()
                                        .insert(k.clone(), v.clone());
                                }
                            } else {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "Dict".to_string(),
                                    found: val.type_name().to_string(),
                                    range: value_node.range,
                                });
                            }
                        }
                        _ => {
                            let key_str = match key {
                                TokenKey::String(s, _, _) => s.clone(),
                                TokenKey::Dynamic(expr_node, _) => {
                                    match self.eval(expr_node, &dict_scope)? {
                                        Value::String(s) => s,
                                        Value::Int(i) => i.to_string(),
                                        Value::Type(t) => {
                                            t.path.first().cloned().unwrap_or_default()
                                        }
                                        other => {
                                            return Err(RuntimeError::TypeMismatch {
                                                expected: "String or Int".to_string(),
                                                found: other.type_name().to_string(),
                                                range: expr_node.range,
                                            })
                                        }
                                    }
                                }
                                _ => key.to_string_key(),
                            };

                            let val = if let Some(thunk) = dict_scope.get_own_thunk(&key_str) {
                                self.force_thunk(&thunk)?
                            } else {
                                let item_scope = dict_scope.with_path(key_str.clone());
                                self.eval(value_node, &item_scope)?
                            };

                            // `#private` keeps the binding in the owning
                            // dict's locals (so siblings can reference it)
                            // but excludes it from the produced `Value::Dict`
                            // — making it invisible to imports, projectors,
                            // and any cross-dict `&root` lookup.
                            if !is_private_field(value_node) {
                                map.insert(key_str.clone(), val.clone());
                            }
                            dict_scope.locals.lock().unwrap().insert(key_str, val);
                        }
                    }
                }
                let result = Value::dict(map);
                self.check_value_size(&result, node.range)?;
                Ok(result)
            }

            Expr::Spread(inner) => self.eval(inner, &current_scope),
            Expr::Comprehension {
                element,
                id,
                iterable,
                condition,
            } => {
                let iter_val = self.eval(iterable, &current_scope)?;
                let items = match iter_val {
                    Value::List(l) => l,
                    _ => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "List".to_string(),
                            found: iter_val.type_name().to_string(),
                            range: iterable.range,
                        })
                    }
                };
                let mut result = Vec::new();
                for item in items.iter() {
                    let mut iter_scope_map = HashMap::new();
                    iter_scope_map.insert(id.clone(), item.clone());
                    let iter_scope = current_scope.with_locals(iter_scope_map);

                    let should_include = if let Some(cond) = condition {
                        self.eval(cond, &iter_scope)?.is_truthy()
                    } else {
                        true
                    };
                    if should_include {
                        result.push(self.eval(element, &iter_scope)?);
                    }
                }
                let result = Value::list(result);
                self.check_value_size(&result, node.range)?;
                Ok(result)
            }
            Expr::Reference { base, path } => {
                self.resolve_reference(base, path, &current_scope, node.range)
            }
            Expr::Variable(path) => self.resolve_variable(path, &current_scope, node.range),
            Expr::Closure {
                params,
                return_type: _,
                body,
            } => {
                let param_names = params.iter().map(|p| p.name.clone()).collect();
                let captured_env = if scope.path_node.is_some() {
                    scope.parent.clone().unwrap_or_else(|| Arc::clone(scope))
                } else {
                    Arc::clone(scope)
                };

                Ok(Value::Closure {
                    params: param_names,
                    body: body.clone(),
                    captured_env,
                })
            }
            Expr::FnCall { path, args } => {
                let mut evaluated_args = Vec::new();
                for arg in args {
                    evaluated_args.push(EvaluatedArg {
                        name: arg.name.clone(),
                        value: self.eval(&arg.value, &current_scope)?,
                    });
                }
                self.call_function(path, evaluated_args, &current_scope, node.range)
            }
            Expr::Binary(Operator::Pipe, left, right) => {
                let left_val = self.eval(left, &current_scope)?;
                match right.expr.as_ref() {
                    Expr::FnCall { path, args } => {
                        let mut evaluated_args = vec![EvaluatedArg::positional(left_val)];
                        for arg in args {
                            evaluated_args.push(EvaluatedArg {
                                name: arg.name.clone(),
                                value: self.eval(&arg.value, &current_scope)?,
                            });
                        }
                        self.call_function(path, evaluated_args, &current_scope, right.range)
                    }
                    _ => {
                        let right_val = self.eval(right, &current_scope)?;
                        if let Value::Closure {
                            params,
                            body,
                            captured_env,
                        } = right_val
                        {
                            self.eval_closure(
                                &params,
                                &body,
                                vec![EvaluatedArg::positional(left_val)],
                                &captured_env,
                                right.range,
                            )
                        } else {
                            Err(RuntimeError::UnsupportedOperator(
                                "Pipe requires a function or closure on the right".to_string(),
                                right.range,
                            ))
                        }
                    }
                }
            }
            Expr::Binary(Operator::And, left, right) => {
                let l = self.eval(left, &current_scope)?;
                if !l.is_truthy() {
                    Ok(l)
                } else {
                    self.eval(right, &current_scope)
                }
            }
            Expr::Binary(Operator::Or, left, right) => {
                let l = self.eval(left, &current_scope)?;
                if l.is_truthy() {
                    Ok(l)
                } else {
                    self.eval(right, &current_scope)
                }
            }
            Expr::Binary(op, left, right) => {
                self.eval_binary(*op, node.range, left, right, &current_scope)
            }
            Expr::Unary(op, inner) => self.eval_unary(*op, node.range, inner, &current_scope),
            Expr::Ternary { cond, then, els } => {
                if self.eval(cond, &current_scope)?.is_truthy() {
                    self.eval(then, &current_scope)
                } else {
                    self.eval(els, &current_scope)
                }
            }
            Expr::Where { expr, bindings } => {
                let bindings_val = self.eval(bindings, &current_scope)?;
                if let Value::Dict(d) = bindings_val {
                    let map_as_hashmap: std::collections::HashMap<String, Value> =
                        d.map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                    let local_scope = current_scope.with_locals(map_as_hashmap);
                    self.eval(expr, &local_scope)
                } else {
                    Err(RuntimeError::TypeMismatch {
                        expected: "Dict".to_string(),
                        found: bindings_val.type_name().to_string(),
                        range: bindings.range,
                    })
                }
            }
            Expr::Match { expr, arms } => {
                let val = self.eval(expr, &current_scope)?;
                for (pattern_node, result_node) in arms {
                    match pattern_node.expr.as_ref() {
                        Expr::Wildcard => {
                            return self.eval(result_node, &current_scope);
                        }
                        Expr::Type(type_node) => {
                            if let Value::Dict(ref d) = val {
                                if let Some(ref brand) = d.brand {
                                    if type_node.path.len() == 1 && &type_node.path[0] == brand {
                                        return self.eval(result_node, &current_scope);
                                    }
                                    let tname = &type_node.path[0];
                                    if !is_builtin_type_name(tname) {
                                        continue;
                                    }
                                }
                            }

                            let mut temp_val = val.clone();
                            if self
                                .check_type(
                                    &mut temp_val,
                                    type_node,
                                    &current_scope,
                                    pattern_node.range,
                                )
                                .is_ok()
                            {
                                return self.eval(result_node, &current_scope);
                            }
                        }
                        _ => {}
                    }
                }
                Err(RuntimeError::TypeMismatch {
                    expected: "a matching arm".to_string(),
                    found: format!("value {}", val),
                    range: node.range,
                })
            }
            Expr::FString(parts) => {
                let mut result = String::new();
                for part in parts {
                    match part {
                        FStringPart::Literal(s) => result.push_str(s),
                        FStringPart::Interpolation(node) => {
                            let val = self.eval(node, &current_scope)?;
                            result.push_str(&format!("{}", val));
                        }
                    }
                }
                Ok(Value::String(result))
            }
            Expr::Type(t) => Ok(Value::Type(t.clone())),
            Expr::Wildcard => Ok(Value::Wildcard),
            Expr::VariantCtor {
                enum_path,
                variant,
                body,
            } => self.eval_variant_ctor(enum_path, variant, body, &current_scope, node.range),
        }?;

        if !is_schema_pred {
            // Post-eval directive transforms (currently `#brand X` on a
            // dict/value). Run before decorators so `@f #brand X v`
            // applies the brand first then `@f`, matching the bottom-up
            // stack order users see for decorators alone.
            for dir in node.directives.iter().rev() {
                if let Some(new_val) = self.apply_directive_post(dir, node, &val, &current_scope)? {
                    val = new_val;
                }
            }
            // Decorators apply bottom-up: `@a @b v ≡ a(b(v))`. The
            // decorator nearest the value wraps first; the outermost
            // wraps last.
            for dec in node.decorators.iter().rev() {
                let name = decorator_name(dec);
                if let Some(plugin) = self.context.decorators.get(&name).cloned() {
                    if let Some(new_val) = plugin.wrap_with_ast(
                        self,
                        node,
                        &val,
                        &current_scope,
                        &dec.args,
                        dec.range,
                    )? {
                        val = new_val;
                        continue;
                    }
                    let dec_args = self.evaluate_call_args(&dec.args, &current_scope)?;
                    val = plugin.wrap(self, val, &current_scope, &dec_args, dec.range)?;
                } else {
                    let dec_args = self.evaluate_call_args(&dec.args, &current_scope)?;
                    val = self.fallback_decorator(
                        &dec.path,
                        val,
                        dec_args,
                        &current_scope,
                        dec.range,
                    )?;
                }
            }
        }

        if let Some(type_hint) = &node.type_hint {
            if !is_schema_pred && !matches!(val, Value::Wildcard) {
                self.check_type(&mut val, type_hint, &current_scope, node.range)?;

                if let Value::Dict(ref mut d) = val {
                    let d = Arc::make_mut(d);
                    d.brand = crate::builtin_decorators::brand_string_for(type_hint);
                }
            }
        }

        Ok(val)
    }

    /// Lower a single `#schema A Body` binding into a `Value::Schema`
    /// (or `Value::EnumSchema` for `Enum<...>` bodies). Mirrors the
    /// path the field-form `#schema X: { ... }` used to take in batch 2:
    /// invoke the analyzer's pure schema lowering, then call
    /// `build_schema_from_def` to bind predicates against the live
    /// scope.
    pub fn lower_schema_binding(
        &self,
        name: &str,
        body: &Node,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        // Fast path: an attached `AnalyzedTree` already split this body
        // into typed fields. Build the runtime `Value::Schema` directly
        // from the pre-computed `SchemaDef`.
        if let Some(tree) = self.context.analyzed.as_ref() {
            if let Some(def) = tree.schema(body.id) {
                if !def.variants.is_empty() {
                    return Ok(self.build_root_enum_schema(def));
                }
                let fields = self.build_schema_from_def(def, scope)?;
                return Ok(Value::Schema {
                    generics: def.generics.clone(),
                    fields,
                });
            }
        }
        let (lowered, _diags) =
            relon_analyzer::lower_schema_pure(Some(name.to_string()), Vec::new(), body);
        let Some(def) = lowered else {
            return Err(RuntimeError::TypeMismatch {
                expected: "schema body (Dict or Enum<...>)".to_string(),
                found: body.expr.kind().to_string(),
                range: body.range,
            });
        };
        if !def.variants.is_empty() {
            return Ok(self.build_root_enum_schema(&def));
        }
        let fields = self.build_schema_from_def(&def, scope)?;
        Ok(Value::Schema {
            generics: def.generics.clone(),
            fields,
        })
    }

    /// Pre-evaluation directive dispatch.
    ///
    /// Currently:
    /// * `#import <spec> from "path"` → loads the module and rescopes
    ///   `current_scope` to expose the imported bindings.
    /// * `#schema A B` (name-body) → seeds the schema name into the
    ///   current scope's locals so the body can reference it before
    ///   walking. At the root level this is also handled by
    ///   `seed_root_schemas`, but doing it here lets nested `#schema`
    ///   directives work too.
    /// * `#schema` (bare, on a dict-field) → interprets the decorated
    ///   value as a schema body instead of data, returning a
    ///   [`Value::Schema`] / [`Value::EnumSchema`] override.
    /// * Everything else → no-op (handled elsewhere).
    ///
    /// Returns `Some(value)` to short-circuit the body evaluation —
    /// only used by the bare `#schema` override path.
    fn apply_directive_pre(
        &self,
        directive: &relon_parser::Directive,
        node: &Node,
        current_scope: &mut Arc<Scope>,
    ) -> Result<Option<Value>, RuntimeError> {
        use crate::decorator_names::{IMPORT, SCHEMA};
        use relon_parser::DirectiveBody;
        match directive.name.as_str() {
            IMPORT => {
                if let DirectiveBody::Import { spec, path, .. } = &directive.body {
                    let new_scope =
                        self.apply_directive_import(spec, path, current_scope, directive.range)?;
                    *current_scope = new_scope;
                }
            }
            // `NameBody` is intentionally skipped here: schema bindings
            // are seeded into the dict's own scope by `prepare_dict_scope`
            // once the body opens. Doing it here too would double-bind
            // and `&sibling` would resolve in the outer scope by mistake.
            SCHEMA => match &directive.body {
                DirectiveBody::NameBody { .. } => {}
                DirectiveBody::Bare => {
                    if let Some(tree) = self.context.analyzed.as_ref() {
                        if let Some(def) = tree.schema(node.id) {
                            if !def.variants.is_empty() {
                                return Ok(Some(self.build_root_enum_schema(def)));
                            }
                            let fields = self.build_schema_from_def(def, current_scope)?;
                            return Ok(Some(Value::Schema {
                                generics: def.generics.clone(),
                                fields,
                            }));
                        }
                    }
                    let (lowered, _diags) =
                        relon_analyzer::lower_schema_pure(None, Vec::new(), node);
                    if let Some(def) = lowered {
                        if !def.variants.is_empty() {
                            return Ok(Some(self.build_root_enum_schema(&def)));
                        }
                        let fields = self.build_schema_from_def(&def, current_scope)?;
                        return Ok(Some(Value::Schema {
                            generics: def.generics.clone(),
                            fields,
                        }));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(None)
    }

    /// Post-evaluation directive dispatch — value transforms only.
    ///
    /// Currently the only post-eval directive transform is `#brand X`,
    /// which mirrors the decorator-form `@brand(X)` from batches 1/2.
    /// Returns `Some(new_val)` to replace the value, or `None` for
    /// pass-through.
    fn apply_directive_post(
        &self,
        directive: &relon_parser::Directive,
        node: &Node,
        value: &Value,
        scope: &Arc<Scope>,
    ) -> Result<Option<Value>, RuntimeError> {
        use crate::decorator_names::BRAND;
        use relon_parser::DirectiveBody;
        if directive.name == BRAND {
            let DirectiveBody::Value(body) = &directive.body else {
                return Ok(None);
            };
            // Reject the ambiguous `Foo x: #brand Bar { ... }` form up
            // front. The outer `Foo` hint and the inner `#brand` would
            // both try to write `dict.brand` (and run their own
            // `check_type`); it's almost never what the author meant.
            if node.type_hint.is_some() {
                return Err(RuntimeError::UnsupportedOperator(
                    "#brand cannot be combined with a field-level type hint on the same value; pick one"
                        .to_string(),
                    directive.range,
                ));
            }
            let type_node = relon_parser::type_node_from_brand_arg(&body.expr, directive.range)
                .ok_or_else(|| {
                    RuntimeError::UnsupportedOperator(
                        "#brand body must be a type name (bareword, string, dotted path, or generic type)"
                            .to_string(),
                        directive.range,
                    )
                })?;
            let mut new_val = value.clone();
            if !matches!(new_val, Value::Wildcard) {
                self.check_type(&mut new_val, &type_node, scope, directive.range)?;
                if let Value::Dict(ref mut d) = new_val {
                    let d = Arc::make_mut(d);
                    d.brand = crate::builtin_decorators::brand_string_for(&type_node);
                }
            }
            return Ok(Some(new_val));
        }
        Ok(None)
    }

    /// Lower a `#import <spec> from "path"` directive into a scope with
    /// the imported bindings.
    pub fn apply_directive_import(
        &self,
        spec: &relon_parser::DirectiveImportSpec,
        path: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Arc<Scope>, RuntimeError> {
        use relon_parser::DirectiveImportSpec;
        let evaluated_module = self.load_module(path, scope, range)?;
        let mut new_locals = HashMap::new();
        match spec {
            DirectiveImportSpec::Alias(name) => {
                new_locals.insert(name.clone(), evaluated_module);
            }
            DirectiveImportSpec::Spread => {
                if let Value::Dict(d) = evaluated_module {
                    for (k, v) in d.map.iter() {
                        new_locals.insert(k.clone(), v.clone());
                    }
                } else {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Dict".to_string(),
                        found: evaluated_module.type_name().to_string(),
                        range,
                    });
                }
            }
            DirectiveImportSpec::Destructure(entries) => {
                let Value::Dict(d) = evaluated_module else {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Dict".to_string(),
                        found: evaluated_module.type_name().to_string(),
                        range,
                    });
                };
                for (name, alias) in entries {
                    let local_name = alias.clone().unwrap_or_else(|| name.clone());
                    let Some(v) = d.map.get(name) else {
                        return Err(RuntimeError::VariableNotFound(
                            format!("`{name}` not exported by `{path}`"),
                            range,
                        ));
                    };
                    new_locals.insert(local_name, v.clone());
                }
            }
        }
        Ok(scope.with_locals(new_locals))
    }

    pub fn evaluate_call_args(
        &self,
        args: &[CallArg],
        scope: &Arc<Scope>,
    ) -> Result<Vec<EvaluatedArg>, RuntimeError> {
        let mut out = Vec::with_capacity(args.len());
        for arg in args {
            out.push(EvaluatedArg {
                name: arg.name.clone(),
                value: self.eval(&arg.value, scope)?,
            });
        }
        Ok(out)
    }

    /// Lower a `#meta ...` directive's body into the positional-args
    /// vector a [`DecoratorPlugin::schema_field_meta`] hook expects.
    ///
    /// * `Bare` → no args.
    /// * `Value(body)` → one positional `EvaluatedArg` carrying the
    ///   eval'd body.
    /// * Other shapes → no args (unsupported here; the analyzer
    ///   guarantees only value/bare shapes reach this path for the meta
    ///   names — `#default`, `#expect`, `#msg`, `#error`, `#brand`).
    pub fn evaluate_directive_meta_args(
        &self,
        directive: &relon_parser::Directive,
        scope: &Arc<Scope>,
    ) -> Result<Vec<EvaluatedArg>, RuntimeError> {
        use relon_parser::DirectiveBody;
        match &directive.body {
            DirectiveBody::Bare => Ok(Vec::new()),
            DirectiveBody::Value(body) => {
                Ok(vec![EvaluatedArg::positional(self.eval(body, scope)?)])
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Resolve `@import("path")` against the registered resolver chain
    /// and evaluate the resulting source. Resolvers are tried in order;
    /// the first one returning `Some(ModuleSource)` wins. Resolved
    /// modules are evaluated with their own `current_dir` so nested
    /// imports inside the module are anchored to the module's location,
    /// not the host's.
    pub fn load_module(
        &self,
        path_str: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let source = self.resolve_module_source(path_str, scope, range)?;
        self.evaluate_module_source(source, range)
    }

    fn resolve_module_source(
        &self,
        path_str: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<ModuleSource, RuntimeError> {
        for resolver in &self.context.module_resolvers {
            if let Some(source) = resolver.resolve(path_str, scope, range)? {
                return Ok(source);
            }
        }
        Err(RuntimeError::ModuleNotFound(
            path_str.to_string(),
            range.into(),
        ))
    }

    fn evaluate_module_source(
        &self,
        source: ModuleSource,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if let Some(cached) = self
            .context
            .module_cache
            .lock()
            .unwrap()
            .get(&source.canonical_id)
        {
            return Ok(cached.clone());
        }
        {
            let loading = self.context.loading_modules.lock().unwrap();
            if loading.contains_key(&source.canonical_id) {
                return Err(RuntimeError::CircularImport(
                    loading.keys().cloned().collect(),
                    range.into(),
                ));
            }
        }
        let _loading_guard = self
            .context
            .enter_loading_module(source.canonical_id.clone());
        let node =
            parse_document(&source.source).map_err(|error| RuntimeError::ModuleParseError {
                path: source.canonical_id.clone(),
                message: error.to_string(),
                range: range.into(),
            })?;
        // Run the analyzer on the imported module so structural errors
        // (missing schema field types, malformed `#main` signatures,
        // duplicate root schemas, etc.) surface at load time rather than
        // being silently accepted. The resulting `AnalyzedTree` is *not*
        // wired into the evaluator yet — the active `Context::analyzed`
        // belongs to the entry file and its `NodeId` keys won't match
        // module-local nodes anyway, so the runtime falls back to
        // `lower_schema_pure` for the module body. That's a known
        // optimization gap; the correctness gain (raising errors instead
        // of swallowing them) is what this pass delivers today.
        let analyzed = relon_analyzer::analyze(&node);
        if analyzed.has_errors() {
            let first_error = analyzed
                .diagnostics
                .iter()
                .find(|d| d.severity() == relon_analyzer::Severity::Error)
                .expect("has_errors() implies at least one Error diagnostic");
            return Err(RuntimeError::ModuleParseError {
                path: source.canonical_id.clone(),
                message: format!("module analyzer reported errors: {first_error}"),
                range: range.into(),
            });
        }
        let module_scope = Arc::new(Scope {
            current_dir: source.current_dir,
            cache_namespace: source.canonical_id.clone(),
            root_ref: Some(crate::scope::RootRef::new(Arc::new(node.clone()))),
            ..Default::default()
        });
        let evaluated = self.eval(&node, &module_scope)?;
        self.context
            .module_cache
            .lock()
            .unwrap()
            .insert(source.canonical_id, evaluated.clone());
        Ok(evaluated)
    }

    pub(crate) fn eval_variant_ctor(
        &self,
        enum_path: &[String],
        variant: &str,
        body: &Node,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let head = enum_path.first().ok_or_else(|| {
            RuntimeError::UnsupportedOperator("variant constructor without enum".into(), range)
        })?;
        let mut current = scope
            .get_local(head)
            .ok_or_else(|| RuntimeError::VariableNotFound(head.clone(), range))?;
        for seg in &enum_path[1..] {
            match current {
                Value::Dict(d) => {
                    current = d.map.get(seg).cloned().ok_or_else(|| {
                        RuntimeError::VariableNotFound(format!("{head}.{seg}"), range)
                    })?;
                }
                _ => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Dict or EnumSchema".into(),
                        found: current.type_name().to_string(),
                        range,
                    })
                }
            }
        }
        let enum_name = enum_path.join(".");
        let Value::EnumSchema { name, variants } = current else {
            return Err(RuntimeError::TypeMismatch {
                expected: format!("EnumSchema `{enum_name}`"),
                found: current.type_name().to_string(),
                range,
            });
        };
        let name = if name.is_empty() {
            enum_name.clone()
        } else {
            name
        };
        let Some(variant_fields) = variants.get(variant) else {
            return Err(RuntimeError::TypeMismatch {
                expected: format!("a variant of `{name}`"),
                found: format!("`{variant}`"),
                range,
            });
        };
        let body_val = self.eval(body, scope)?;
        let Value::Dict(body_dict) = body_val else {
            return Err(RuntimeError::TypeMismatch {
                expected: "Dict variant body".into(),
                found: "non-Dict".into(),
                range,
            });
        };
        let mut map = match Arc::try_unwrap(body_dict) {
            Ok(d) => d.map,
            Err(arc) => arc.map.clone(),
        };
        for (fname, field_def) in variant_fields.iter() {
            if let Some(fval) = map.get_mut(fname) {
                self.check_type(fval, &field_def.type_hint, scope, range)?;
            } else if field_def.type_hint.is_optional {
                continue;
            } else if let Some(default) = &field_def.default_value {
                map.insert(fname.clone(), default.clone());
            } else {
                return Err(RuntimeError::TypeMismatch {
                    expected: format!("field `{fname}` for variant `{variant}`"),
                    found: "missing".into(),
                    range,
                });
            }
        }
        Ok(Value::variant_dict(map, variant.to_string(), name))
    }

    pub(crate) fn call_function_by_value(
        &self,
        func: Value,
        args: Vec<EvaluatedArg>,
        _scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        match func {
            Value::Closure {
                params,
                body,
                captured_env,
            } => self.eval_closure(&params, &body, args, &captured_env, range),
            _ => Err(RuntimeError::TypeMismatch {
                expected: "Closure".to_string(),
                found: func.type_name().to_string(),
                range,
            }),
        }
    }

    fn call_function(
        &self,
        path: &[TokenKey],
        args: Vec<EvaluatedArg>,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if let Ok(Value::Closure {
            params,
            body,
            captured_env,
        }) = self.resolve_variable(path, scope, range)
        {
            return self.eval_closure(&params, &body, args, &captured_env, range);
        }
        if let Some(name) = Self::native_function_name(path) {
            if let Some(entry) = self.context.functions.get(&name) {
                self.check_native_fn_capability(&name, entry, range)?;
                return entry
                    .func
                    .call(NativeArgs::from_evaluated(args, self.caps()), range);
            }
        }
        Err(RuntimeError::FunctionNotFound(
            path.iter()
                .map(|k| k.to_string_key())
                .collect::<Vec<_>>()
                .join("."),
            range,
        ))
    }

    fn check_native_fn_capability(
        &self,
        name: &str,
        entry: &GatedNativeFn,
        range: TokenRange,
    ) -> Result<(), RuntimeError> {
        if !entry.gated {
            return Ok(());
        }
        let caps = &self.context.capabilities;
        if caps.allow_all_native_fn || caps.allow_native_fn.contains(name) {
            return Ok(());
        }
        Err(RuntimeError::CapabilityDenied {
            name: name.to_string(),
            reason: if entry.gate.reads_fs {
                "function declared `reads_fs` but is not in the sandbox allowlist".to_string()
            } else {
                "function not in sandbox allowlist".to_string()
            },
            range,
        })
    }

    fn fallback_decorator(
        &self,
        path: &[TokenKey],
        value: Value,
        args: Vec<EvaluatedArg>,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if let Ok(Value::Closure {
            params,
            body,
            captured_env,
        }) = self.resolve_variable(path, scope, range)
        {
            let mut combined = vec![EvaluatedArg::positional(value)];
            combined.extend(args);
            return self.eval_closure(&params, &body, combined, &captured_env, range);
        }
        if let Some(name) = Self::native_function_name(path) {
            if let Some(entry) = self.context.functions.get(&name) {
                self.check_native_fn_capability(&name, entry, range)?;
                let mut native = NativeArgs::from_evaluated(args, self.caps());
                native.positional.insert(0, value);
                return entry.func.call(native, range);
            }
        }
        Err(RuntimeError::UnsupportedOperator(
            format!(
                "Decorator @{} not found",
                path.iter()
                    .map(|k| k.to_string_key())
                    .collect::<Vec<_>>()
                    .join(".")
            ),
            range,
        ))
    }

    fn native_function_name(path: &[TokenKey]) -> Option<String> {
        let mut parts = Vec::with_capacity(path.len());
        for part in path {
            match part {
                TokenKey::String(name, _, _) => parts.push(name.as_str()),
                _ => return None,
            }
        }
        Some(parts.join("."))
    }

    fn eval_closure(
        &self,
        params: &[String],
        body: &Node,
        args: Vec<EvaluatedArg>,
        captured_env: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let mut bindings = HashMap::new();
        let mut pos_idx = 0;
        for arg in &args {
            if arg.name.is_none() {
                if pos_idx < params.len() {
                    bindings.insert(params[pos_idx].clone(), arg.value.clone());
                    pos_idx += 1;
                } else {
                    return Err(RuntimeError::TypeMismatch {
                        expected: format!("at most {}", params.len()),
                        found: "more".to_string(),
                        range,
                    });
                }
            }
        }
        for arg in &args {
            if let Some(name) = &arg.name {
                if !params.contains(name) {
                    return Err(RuntimeError::VariableNotFound(name.clone(), range));
                }
                if bindings.contains_key(name) {
                    return Err(RuntimeError::UnsupportedOperator(
                        format!("Duplicate {}", name),
                        range,
                    ));
                }
                bindings.insert(name.clone(), arg.value.clone());
            }
        }
        if bindings.len() < params.len() {
            return Err(RuntimeError::TypeMismatch {
                expected: format!("{}", params.len()),
                found: format!("{}", bindings.len()),
                range,
            });
        }
        // Each closure invocation gets a fresh cache namespace so that
        // path-cache entries built while evaluating the body (notably
        // `&sibling.<x>` lookups, which key off `cache_namespace`) are
        // not shared across calls with different bound parameters. See
        // `Context::closure_call_counter`.
        let call_id = self
            .context
            .closure_call_counter
            .fetch_add(1, Ordering::Relaxed);
        let call_namespace = if captured_env.cache_namespace.is_empty() {
            format!("closure#{call_id}")
        } else {
            format!("{}#call{}", captured_env.cache_namespace, call_id)
        };
        let bindings_scope = Arc::new(Scope {
            parent: Some(Arc::clone(captured_env)),
            path_node: None,
            locals: Mutex::new(bindings),
            current_dir: captured_env.current_dir.clone(),
            cache_namespace: call_namespace,
            root_ref: captured_env.root_ref.clone(),
            list_context: None,
            thunks: Mutex::new(HashMap::new()),
        });
        let body_arc = Arc::new(body.clone());
        let body_scope = Arc::new(Scope {
            parent: Some(Arc::clone(&bindings_scope)),
            path_node: None,
            locals: Mutex::new(HashMap::new()),
            current_dir: bindings_scope.current_dir.clone(),
            cache_namespace: bindings_scope.cache_namespace.clone(),
            root_ref: Some(crate::scope::RootRef {
                node: Arc::clone(&body_arc),
                scope: None,
                parent_fallback: Some(bindings_scope.clone()),
            }),
            list_context: None,
            thunks: Mutex::new(HashMap::new()),
        });
        self.eval(&body_arc, &body_scope)
    }

    pub(crate) fn prepare_dict_scope(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
    ) -> Result<(), RuntimeError> {
        if let Expr::Dict(pairs) = node.expr.as_ref() {
            self.register_dict_thunks(pairs, scope)?;

            // Run any `#schema A Body` directives stacked above this
            // dict so the bindings land in `scope.locals` before the
            // body — siblings should be able to reference them by name.
            // (Root-level schema directives are also handled by
            // `seed_root_schemas`; that's intentional duplication: this
            // pass also covers nested dicts.)
            // Two-phase schema directive binding: first seed every
            // declared name with a placeholder so cross-references
            // (`#schema A ...; #schema B &sibling.A + ...`) and
            // re-entry from `&sibling`/`&root` walks see the name as
            // already-bound (placeholder), preventing infinite
            // recursion through this same prepare-phase. Then walk
            // again to actually build each schema's value.
            let mut seeded: HashSet<&str> = HashSet::new();
            {
                let mut locals = scope.locals.lock().unwrap();
                for dir in &node.directives {
                    if dir.name != crate::decorator_names::SCHEMA {
                        continue;
                    }
                    let relon_parser::DirectiveBody::NameBody { name, .. } = &dir.body else {
                        continue;
                    };
                    if locals.contains_key(name) {
                        continue;
                    }
                    locals.insert(
                        name.clone(),
                        Value::Schema {
                            generics: Vec::new(),
                            fields: HashMap::new(),
                        },
                    );
                    seeded.insert(name);
                }
            }
            for dir in &node.directives {
                if dir.name != crate::decorator_names::SCHEMA {
                    continue;
                }
                let relon_parser::DirectiveBody::NameBody { name, body, .. } = &dir.body else {
                    continue;
                };
                if !seeded.contains(name.as_str()) {
                    continue;
                }
                let val = self.lower_schema_binding(name, body, scope)?;
                scope.locals.lock().unwrap().insert(name.clone(), val);
            }

            for (key, value_node) in pairs {
                if matches!(key, TokenKey::Spread(_)) {
                    continue;
                }
                let has_schema_directive = value_node.directives.iter().any(|d| {
                    d.name == crate::decorator_names::SCHEMA
                        && matches!(d.body, relon_parser::DirectiveBody::Bare)
                });
                let is_dict_schema =
                    has_schema_directive && matches!(value_node.expr.as_ref(), Expr::Dict(_));
                let is_enum_schema = has_schema_directive
                    && matches!(value_node.expr.as_ref(),
                        Expr::Type(t) if t.path.len() == 1 && t.path[0] == "Enum");

                if Self::is_logic_definition(value_node) || is_dict_schema || is_enum_schema {
                    let key_str = match key {
                        TokenKey::String(s, _, _) => s.clone(),
                        TokenKey::Dynamic(expr_node, _) => match self.eval(expr_node, scope)? {
                            Value::String(s) => s,
                            Value::Int(i) => i.to_string(),
                            Value::Type(t) => t.path.first().cloned().unwrap_or_default(),
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "String or Int for key".to_string(),
                                    found: other.type_name().to_string(),
                                    range: expr_node.range,
                                })
                            }
                        },
                        _ => key.to_string_key(),
                    };
                    if !Self::is_valid_identifier(&key_str) {
                        return Err(RuntimeError::InvalidIdentifier(key_str, value_node.range));
                    }
                    if is_dict_schema {
                        let mut generics = Vec::new();
                        if let TokenKey::Dynamic(expr_node, _) = key {
                            if let Expr::Type(t) = expr_node.expr.as_ref() {
                                generics = t
                                    .generics
                                    .iter()
                                    .filter_map(|g| g.path.first().cloned())
                                    .collect();
                            }
                        }
                        scope.locals.lock().unwrap().insert(
                            key_str.clone(),
                            Value::Schema {
                                generics,
                                fields: HashMap::new(),
                            },
                        );
                    }
                    let val = self.eval(value_node, scope)?;
                    scope.locals.lock().unwrap().insert(key_str, val);
                }
            }
        }
        Ok(())
    }

    fn register_dict_thunks(
        &self,
        pairs: &[(TokenKey, Node)],
        scope: &Arc<Scope>,
    ) -> Result<(), RuntimeError> {
        // Resolve every dynamic key in a separate pass *before* taking
        // the thunks lock. Two reasons:
        //   1. `self.eval(expr_node, scope)` can recursively re-enter
        //      this scope (variable lookups, sub-dict preparation, …),
        //      and the resulting `scope.get_thunk(...)` would dead-lock
        //      on the same `Mutex`.
        //   2. Errors from dynamic-key evaluation must surface here —
        //      previously the code did `_ => continue`, silently
        //      dropping the thunk and forcing the caller to re-evaluate
        //      the same expression later (and re-encounter the same
        //      error). Fail-fast keeps the prepare-phase invariant
        //      "thunks table covers every declared key" honest.
        let mut entries: Vec<(String, Node)> = Vec::with_capacity(pairs.len());
        for (key, value_node) in pairs {
            let key_str = match key {
                TokenKey::String(s, _, _) => s.clone(),
                TokenKey::Dummy => "_".to_string(),
                TokenKey::Index(i, _) => i.to_string(),
                TokenKey::Spread(_) => continue,
                TokenKey::Dynamic(expr_node, _) => match self.eval(expr_node, scope)? {
                    Value::String(s) => s,
                    Value::Int(i) => i.to_string(),
                    Value::Type(t) => t.path.first().cloned().unwrap_or_default(),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "String or Int for key".to_string(),
                            found: other.type_name().to_string(),
                            range: expr_node.range,
                        });
                    }
                },
            };
            entries.push((key_str, value_node.clone()));
        }

        let mut thunks = scope.thunks.lock().unwrap();
        for (key_str, value_node) in entries {
            let item_scope = scope.with_path(key_str.clone());
            let path = item_scope.full_path();
            let cache_key = item_scope.path_cache_key(&path);
            thunks.insert(
                key_str,
                Arc::new(Thunk::new(value_node, item_scope, path, cache_key)),
            );
        }
        Ok(())
    }
}

/// Caps handle handed to native functions so they can call back into Relon.
///
/// Holds an `Arc<Context>` so the trait object is `'static` and the call-back
/// path can run a fresh [`Evaluator`] over the same shared context. Cheap to
/// keep around — every clone is just an Arc bump.
struct EvaluatorCaps {
    context: Arc<Context>,
}

impl NativeFnCaps for EvaluatorCaps {
    fn call_relon(
        &self,
        func: &Value,
        args: Vec<Value>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let evaluator = Evaluator::new(Arc::clone(&self.context));
        let evaluated_args = args.into_iter().map(EvaluatedArg::positional).collect();
        let scope = Arc::clone(evaluator.empty_scope());
        evaluator.call_function_by_value(func.clone(), evaluated_args, &scope, range)
    }
}

pub(crate) fn decorator_name(dec: &DecoratorNode) -> String {
    dec.path
        .iter()
        .map(|k| k.to_string_key())
        .collect::<Vec<_>>()
        .join(".")
}

/// True when `node` carries the `#private` directive. See
/// [`crate::decorator_names::PRIVATE`] for the field-level semantics.
pub(crate) fn is_private_field(node: &Node) -> bool {
    node.directives
        .iter()
        .any(|dir| dir.name == crate::decorator_names::PRIVATE)
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(fl) => write!(f, "{}", fl),
            Value::String(s) => write!(f, "{}", s),
            Value::List(l) => write!(f, "{:?}", l),
            Value::Dict(d) => write!(f, "{:?}", d.map),
            Value::Closure { .. } => write!(f, "<closure>"),
            Value::Schema { .. } => write!(f, "<schema>"),
            Value::EnumSchema { name, .. } => write!(f, "<enum {name}>"),
            Value::Type(t) => write!(f, "Type<{}>", crate::schema::format_type_node(t)),
            Value::Wildcard => write!(f, "*"),
        }
    }
}
