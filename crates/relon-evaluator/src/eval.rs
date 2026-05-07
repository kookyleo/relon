use crate::decorator::{DecoratorPlugin, PreEvalOutcome};
use crate::error::RuntimeError;
use crate::module::{FilesystemModuleResolver, ModuleResolver, ModuleSource, StdModuleResolver};
use crate::native_fn::{EvaluatedArg, NativeArgs, RelonFunction, NativeFnCaps};
use crate::scope::{Scope, Thunk};
use crate::value::Value;
use relon_parser::{
    is_builtin_type_name, parse_document, CallArg, Decorator as DecoratorNode, Expr, FStringPart,
    Node, Operator, TokenKey, TokenRange,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
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
    pub globals: HashMap<String, Value>,
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
    /// or [`Self::trusted`] for context shapes intended for real workloads.
    pub fn new() -> Self {
        let mut this = Self {
            root_node: None,
            decorators: HashMap::new(),
            functions: HashMap::new(),
            globals: HashMap::new(),
            schemas: HashMap::new(),
            module_resolvers: Vec::new(),
            path_cache: Mutex::new(HashMap::new()),
            module_cache: Mutex::new(HashMap::new()),
            loading_modules: Mutex::new(HashMap::new()),
            evaluating_paths: Mutex::new(HashSet::new()),
            step_counter: AtomicU64::new(0),
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
    /// **Sandbox scope:** this only constrains filesystem `@import` and
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

pub struct Evaluator<'a> {
    pub context: &'a Context,
    /// Lazy cache for the `Arc<dyn NativeFnCaps>` handed to native fns so
    /// closures can call back into Relon. Allocating one per call shows up
    /// in the per-element hot path of `_list_map`/`_list_filter` etc.
    caps: std::sync::OnceLock<Arc<EvaluatorCaps>>,
    /// Cached empty scope used as the parent of native-fn closure
    /// callbacks. Avoids one `Arc::new(Scope::default())` per element.
    empty_scope: std::sync::OnceLock<Arc<Scope>>,
}

impl<'a> Evaluator<'a> {
    pub fn new(context: &'a Context) -> Self {
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
                    evaluator: self as *const Evaluator as usize,
                })
            })
            .clone()
    }

    fn empty_scope(&self) -> &Arc<Scope> {
        self.empty_scope
            .get_or_init(|| Arc::new(Scope::default()))
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

    /// Evaluate the document attached to `Context::with_root`. Stamps
    /// `scope.reference_root` with the root node so `&root` references
    /// resolve correctly during the walk; the reference-equality check
    /// against the existing `reference_root` lets nested modules
    /// preserve their own root binding when re-entering this from
    /// `load_module`.
    pub fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        let root = self.context.root_node.clone().ok_or_else(|| {
            RuntimeError::VariableNotFound(
                "Context has no root node — call `Context::with_root` first".to_string(),
                TokenRange::default(),
            )
        })?;
        let scope = if scope.reference_root.is_none() {
            let mut overlay = (**scope).clone();
            overlay.reference_root = Some(Arc::clone(&root));
            Arc::new(overlay)
        } else {
            Arc::clone(scope)
        };
        self.eval(&root, &scope)
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
                    .reference_root
                    .as_ref()
                    .is_some_and(|r| std::ptr::eq(r.as_ref() as *const _, node as *const _));

                let mut dict_scope = Arc::new(Scope {
                    parent: Some(Arc::clone(&current_scope)),
                    path_node: None,
                    locals: Mutex::new(HashMap::new()),
                    current_dir: current_scope.current_dir.clone(),
                    cache_namespace: current_scope.cache_namespace.clone(),
                    reference_root: current_scope.reference_root.clone(),
                    reference_root_parent: current_scope.reference_root_parent.clone(),
                    reference_root_scope: current_scope.reference_root_scope.clone(),
                    list_context: current_scope.list_context.clone(),
                    thunks: Mutex::new(HashMap::new()),
                });

                if is_root {
                    let mut modified = (*dict_scope).clone();
                    modified.reference_root_scope = Some(dict_scope.clone());
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
                                        Value::Type(t) => t.path.first().cloned().unwrap_or_default(),
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

                            if !key_str.starts_with('_') || !matches!(val, Value::Closure { .. }) {
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
            Expr::Binary(op, left, right) => self.eval_binary(*op, left, right, &current_scope),
            Expr::Unary(op, node) => self.eval_unary(*op, node, &current_scope),
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
            for dec in &node.decorators {
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

    pub fn apply_import(
        &self,
        args: &[CallArg],
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Arc<Scope>, RuntimeError> {
        let mut path_str = String::new();
        let mut alias: Option<String> = None;
        let mut should_spread = false;
        for arg in args {
            let val = self.eval(&arg.value, scope)?;
            match arg.name.as_deref() {
                Some("path") | None if path_str.is_empty() => {
                    if let Value::String(s) = val {
                        path_str = s;
                    }
                }
                Some("as") => {
                    if let Value::String(s) = val {
                        alias = Some(s);
                    }
                }
                Some("spread") => {
                    if let Value::Bool(b) = val {
                        should_spread = b;
                    }
                }
                _ => {}
            }
        }
        let evaluated_module = self.load_module(&path_str, scope, range)?;
        let final_alias = if let Some(a) = alias {
            Some(a)
        } else if !should_spread {
            Path::new(&path_str)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        } else {
            None
        };
        let mut new_locals = HashMap::new();
        if let Some(a) = final_alias {
            new_locals.insert(a, evaluated_module.clone());
        }
        if should_spread {
            if let Value::Dict(d) = evaluated_module {
                for (k, v) in d.map.iter() {
                    if !k.starts_with('_') {
                        new_locals.insert(k.clone(), v.clone());
                    }
                }
            } else {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Dict".to_string(),
                    found: evaluated_module.type_name().to_string(),
                    range,
                });
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
        let module_scope = Arc::new(Scope {
            current_dir: source.current_dir,
            cache_namespace: source.canonical_id.clone(),
            reference_root: Some(Arc::new(node.clone())),
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
            } => {
                let mut local_vars = HashMap::new();
                for (i, param_name) in params.iter().enumerate() {
                    if let Some(arg) = args.get(i) {
                        local_vars.insert(param_name.clone(), arg.value.clone());
                    } else {
                        return Err(RuntimeError::TypeMismatch {
                            expected: format!("at least {} arguments", params.len()),
                            found: format!("{}", args.len()),
                            range,
                        });
                    }
                }
                let call_scope = captured_env.with_locals(local_vars);
                self.eval(&body, &call_scope)
            }
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
        let bindings_scope = Arc::new(Scope {
            parent: Some(Arc::clone(captured_env)),
            path_node: None,
            locals: Mutex::new(bindings),
            current_dir: captured_env.current_dir.clone(),
            cache_namespace: captured_env.cache_namespace.clone(),
            reference_root: captured_env.reference_root.clone(),
            reference_root_parent: captured_env.reference_root_parent.clone(),
            reference_root_scope: captured_env.reference_root_scope.clone(),
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
            reference_root: Some(Arc::clone(&body_arc)),
            reference_root_parent: Some(bindings_scope.clone()),
            reference_root_scope: None,
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
            self.register_dict_thunks(pairs, scope);
            for (key, value_node) in pairs {
                if matches!(key, TokenKey::Spread(_)) {
                    continue;
                }
                let is_schema = value_node.decorators.iter().any(|d| {
                    let name = d
                        .path
                        .iter()
                        .map(|k| k.to_string_key())
                        .collect::<Vec<_>>()
                        .join(".");
                    name == "schema"
                });

                let is_dict_schema = is_schema && matches!(value_node.expr.as_ref(), Expr::Dict(_));
                let is_enum_schema = is_schema
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

    fn register_dict_thunks(&self, pairs: &[(TokenKey, Node)], scope: &Arc<Scope>) {
        let mut thunks = scope.thunks.lock().unwrap();
        for (key, value_node) in pairs {
            let key_str = match key {
                TokenKey::String(s, _, _) => s.clone(),
                TokenKey::Dummy => "_".to_string(),
                TokenKey::Index(i, _) => i.to_string(),
                TokenKey::Spread(_) => continue,
                TokenKey::Dynamic(expr_node, _) => match self.eval(expr_node, scope) {
                    Ok(Value::String(s)) => s,
                    Ok(Value::Int(i)) => i.to_string(),
                    Ok(Value::Type(t)) => t.path.first().cloned().unwrap_or_default(),
                    _ => continue,
                },
            };
            let item_scope = scope.with_path(key_str.clone());
            let path = item_scope.full_path();
            let cache_key = item_scope.path_cache_key(&path);
            thunks.insert(
                key_str,
                Arc::new(Thunk::new(value_node.clone(), item_scope, path, cache_key)),
            );
        }
    }
}

/// Caps handle handed to native functions so they can call back into
/// Relon. Holds a raw pointer to the `Evaluator` because the trait object
/// needs to be `'static` while `Evaluator<'a>` is not.
///
/// SAFETY contract: the caps handle is created lazily by `Evaluator::caps`
/// and stored in a `OnceLock` on the same `Evaluator`. The pointer is
/// dereferenced only inside `call_relon`, which the host invokes
/// synchronously from a `RelonFunction::call`. Callers must not store the
/// `Arc<dyn NativeFnCaps>` past the lifetime of the originating
/// `Evaluator` — `RelonFunction` impls in this crate do not, and host
/// impls are required to follow the same rule.
struct EvaluatorCaps {
    /// Type-erased `*const Evaluator<'_>`. Stored as `usize` because the
    /// trait object handed to native functions must be `'static` while
    /// `Evaluator<'a>` is not. Reconstituted in `call_relon`.
    evaluator: usize,
}

impl NativeFnCaps for EvaluatorCaps {
    fn call_relon(
        &self,
        func: &Value,
        args: Vec<Value>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        // SAFETY: see struct-level contract.
        let evaluator = unsafe { &*(self.evaluator as *const Evaluator) };
        let evaluated_args = args.into_iter().map(EvaluatedArg::positional).collect();
        evaluator.call_function_by_value(
            func.clone(),
            evaluated_args,
            evaluator.empty_scope(),
            range,
        )
    }
}

pub(crate) fn decorator_name(dec: &DecoratorNode) -> String {
    dec.path
        .iter()
        .map(|k| k.to_string_key())
        .collect::<Vec<_>>()
        .join(".")
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
