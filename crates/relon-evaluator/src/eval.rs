use crate::decorator::{DecoratorPlugin, PreEvalOutcome};
use crate::error::RuntimeError;
use crate::module::{FilesystemModuleResolver, ModuleResolver, ModuleSource, StdModuleResolver};
use crate::native_fn::{EvaluatedArg, NativeArgs, RelonFunction};
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

/// Capability gates wired into `Context`. Defaults to **fully sandboxed**:
/// no native fns, zero-budget step counter, zero-size values. Hosts opt in
/// by either populating the fields directly or by constructing a `Context`
/// via [`Context::trusted`] (which flips `allow_all_native_fn` on and
/// leaves the limits unbounded).
///
/// Filesystem read policy lives on the [`FilesystemModuleResolver`]
/// (configured via `with_root_dir` / `trusted`), not here.
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    /// Hard cap on `eval_internal` invocations. `None` = unbounded.
    pub max_steps: Option<u64>,
    /// Hard cap on the *element count* of any single `Value::List` /
    /// `Value::Dict` produced by the evaluator. The field is named
    /// `_bytes` for forward compatibility with a future byte-accurate
    /// metric, but today we measure element count (per the spec).
    pub max_value_bytes: Option<usize>,
    /// Names allowed for native-function calls under sandbox. Functions
    /// registered via the legacy [`Context::register_fn`] are treated as
    /// trusted — the gate fires only for functions registered via
    /// [`Context::register_fn_with_caps`]. Empty = no gated functions
    /// allowed.
    pub allow_native_fn: HashSet<String>,
    /// Trusted-mode escape hatch: when `true`, every native function
    /// registered via [`Context::register_fn_with_caps`] is accepted
    /// regardless of `allow_native_fn`. Set by [`Context::trusted`].
    pub allow_all_native_fn: bool,
}

/// Capability declaration for a host-registered native function. Used by
/// the registry to decide whether a sandboxed call is allowed; left wide
/// open so we can grow the surface (`writes_fs`, `network`, `env`, …)
/// without breaking the existing API.
#[derive(Debug, Clone, Default)]
pub struct NativeFnCaps {
    pub reads_fs: bool,
}

/// Internal record for a host-registered function, pairing the callable
/// with the caps it declared at registration time.
pub(crate) struct GatedNativeFn {
    pub(crate) func: Arc<dyn RelonFunction>,
    pub(crate) caps: NativeFnCaps,
    /// Only legacy `register_fn` produces records with `gated == false`.
    /// Those bypass the sandbox check entirely (matches the spec: "treats
    /// fn as fully-trusted, equivalent to all caps true").
    pub(crate) gated: bool,
}

pub struct Context {
    pub globals: HashMap<String, Value>,
    pub(crate) functions: HashMap<String, GatedNativeFn>,
    pub decorators: HashMap<String, Arc<dyn DecoratorPlugin>>,
    /// Ordered chain of module resolvers consulted by `@import`. The first
    /// resolver returning `Some(ModuleSource)` wins. Default chain: built-in
    /// `std/...` virtual modules, then the local filesystem.
    pub module_resolvers: Vec<Arc<dyn ModuleResolver>>,
    pub root_node: Option<Arc<Node>>,
    /// Output of the `relon-analyzer` semantic pass over `root_node`, if
    /// the host opted in via [`Context::with_analyzed`]. Decorator plugins
    /// and the type checker can use it as a fast-path side-table; the
    /// evaluator never *requires* it (and falls back to its own
    /// extraction logic when absent).
    pub analyzed: Option<Arc<relon_analyzer::AnalyzedTree>>,
    /// Sandbox / capability gates. See [`Capabilities`].
    pub capabilities: Capabilities,
    /// Number of times `eval_internal` has been entered for this context.
    /// Atomic so the counter survives without coarsening evaluator borrows;
    /// the evaluator is single-threaded but `Context` is `Send + Sync`.
    pub(crate) step_counter: AtomicU64,
    pub module_cache: Mutex<HashMap<String, Value>>,
    pub(crate) path_cache: Mutex<HashMap<String, Value>>,
    pub(crate) evaluating_paths: Mutex<HashSet<String>>,
    pub(crate) loading_modules: Mutex<Vec<String>>,
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// Trusted constructor. Equivalent to [`Context::trusted`]; preserved
    /// for backwards compatibility with the ~163 tests and host call sites
    /// that predate the sandbox model. **Use [`Context::sandboxed`] as the
    /// starting point for any untrusted script.**
    pub fn new() -> Self {
        Self::trusted()
    }

    /// Wide-open context: filesystem reads enabled with no root, unbounded
    /// step / value budgets, every native function callable. This is what
    /// the legacy [`Context::new`] returns; spell it explicitly when the
    /// caller wants to make the trust level visible.
    pub fn trusted() -> Self {
        let caps = Capabilities {
            allow_all_native_fn: true,
            ..Capabilities::default()
        };
        Self::with_capabilities_and_resolvers(
            caps,
            vec![
                Arc::new(StdModuleResolver),
                Arc::new(FilesystemModuleResolver::trusted()),
            ],
        )
    }

    /// Fully sandboxed context: filesystem reads default-rejected (no
    /// root), capabilities at their restrictive defaults, only the
    /// virtual `std/...` resolver wired up. Hosts grant capabilities by
    /// mutating `capabilities`, swapping in a rooted
    /// [`FilesystemModuleResolver`], and registering native functions
    /// with [`Context::register_fn_with_caps`].
    pub fn sandboxed() -> Self {
        Self::with_capabilities_and_resolvers(
            Capabilities::default(),
            vec![
                Arc::new(StdModuleResolver),
                // Default-rejecting filesystem resolver. Swap or augment
                // via `prepend_module_resolver` once the host knows what
                // root to expose.
                Arc::new(FilesystemModuleResolver::default()),
            ],
        )
    }

    fn with_capabilities_and_resolvers(
        capabilities: Capabilities,
        module_resolvers: Vec<Arc<dyn ModuleResolver>>,
    ) -> Self {
        let mut ctx = Self {
            globals: HashMap::new(),
            functions: HashMap::new(),
            decorators: HashMap::new(),
            module_resolvers,
            root_node: None,
            analyzed: None,
            capabilities,
            step_counter: AtomicU64::new(0),
            module_cache: Mutex::new(HashMap::new()),
            path_cache: Mutex::new(HashMap::new()),
            evaluating_paths: Mutex::new(HashSet::new()),
            loading_modules: Mutex::new(Vec::new()),
        };
        crate::stdlib::register_to(&mut ctx);
        crate::builtin_decorators::register_to(&mut ctx);
        ctx
    }

    /// Replace the current capability set wholesale. Builder-style so
    /// hosts can chain it onto `Context::sandboxed()`.
    pub fn with_capabilities(mut self, caps: Capabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Insert a [`ModuleResolver`] at the front of the resolver chain. Use
    /// this when the host wants to intercept imports before built-in
    /// `std/...` and filesystem lookups.
    pub fn prepend_module_resolver(&mut self, resolver: Arc<dyn ModuleResolver>) {
        self.module_resolvers.insert(0, resolver);
    }

    pub fn enter_loading_module<S: Into<String>>(&self, path: S) -> LoadingModuleGuard<'_> {
        let path = path.into();
        self.loading_modules.lock().unwrap().push(path.clone());
        LoadingModuleGuard {
            loading_modules: &self.loading_modules,
            path,
        }
    }

    pub fn with_root(mut self, root: Node) -> Self {
        self.root_node = Some(Arc::new(root));
        self
    }

    /// Attach a pre-computed [`relon_analyzer::AnalyzedTree`] so decorator
    /// plugins and the type checker can fast-path through it. Pass the
    /// same root node to `analyze` first, then forward both into the
    /// evaluator.
    pub fn with_analyzed(mut self, analyzed: Arc<relon_analyzer::AnalyzedTree>) -> Self {
        self.analyzed = Some(analyzed);
        self
    }

    /// Resolve a reference site (`Reference { ... }` or `Variable(...)`
    /// node id) to the target value-node the analyzer bound it to.
    ///
    /// Returns `None` when the analyzer wasn't attached, didn't visit
    /// the node, or couldn't statically determine a target. Hosts and
    /// tooling (LSP go-to-definition, type checkers, debuggers) call
    /// this so they share the evaluator's view of "what does this
    /// reference point to". The evaluator itself doesn't use this on
    /// its hot path — its thunk + cache machinery is already doing the
    /// equivalent walk with the cycle protection a fast-path would
    /// have to re-implement.
    pub fn analyzer_target(&self, site_id: relon_parser::NodeId) -> Option<Arc<Node>> {
        let tree = self.analyzed.as_ref()?;
        let resolved = tree.references.get(&site_id)?;
        tree.node_index.get(&resolved.target).cloned()
    }

    /// Register a fully-trusted native function. Calls bypass the sandbox
    /// gate — use [`Context::register_fn_with_caps`] for anything that
    /// needs to be guarded.
    pub fn register_fn<S: Into<String>>(&mut self, name: S, f: Arc<dyn RelonFunction>) {
        self.functions.insert(
            name.into(),
            GatedNativeFn {
                func: f,
                caps: NativeFnCaps::default(),
                gated: false,
            },
        );
    }

    /// Register a native function whose calls are gated by the current
    /// capability set. In sandbox mode the call is rejected unless either
    /// `Capabilities::allow_all_native_fn` is on or `name` appears in
    /// `Capabilities::allow_native_fn`.
    pub fn register_fn_with_caps<S: Into<String>>(
        &mut self,
        name: S,
        caps: NativeFnCaps,
        f: Arc<dyn RelonFunction>,
    ) {
        self.functions.insert(
            name.into(),
            GatedNativeFn {
                func: f,
                caps,
                gated: true,
            },
        );
    }

    /// Register a decorator plugin under `name` (the full dotted path,
    /// e.g. `"ensure.int"`). Replaces any previously registered plugin with
    /// the same name, which is how hosts override built-ins.
    pub fn register_decorator<S: Into<String>>(
        &mut self,
        name: S,
        plugin: Arc<dyn DecoratorPlugin>,
    ) {
        self.decorators.insert(name.into(), plugin);
    }
}

pub struct Evaluator<'a> {
    pub context: &'a Context,
}

pub struct LoadingModuleGuard<'a> {
    loading_modules: &'a Mutex<Vec<String>>,
    path: String,
}

impl Drop for LoadingModuleGuard<'_> {
    fn drop(&mut self) {
        let mut loading_modules = self.loading_modules.lock().unwrap();
        if let Some(index) = loading_modules
            .iter()
            .rposition(|module| module == &self.path)
        {
            loading_modules.remove(index);
        }
    }
}

impl<'a> Evaluator<'a> {
    pub fn new(context: &'a Context) -> Self {
        Self { context }
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
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    fn is_logic_definition(node: &Node) -> bool {
        matches!(node.expr.as_ref(), Expr::Closure { .. })
    }

    pub fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        self.eval_internal(node, scope, false)
    }

    /// Reject values whose top-level element count exceeds the configured
    /// `max_value_bytes` limit. Called from the construction sites where
    /// lists/dicts grow (literal evaluation, arithmetic merge, spread, etc.):
    /// threading `&Context` into `Value::list_mut`/`dict_mut` would balloon
    /// the API for what's effectively a single bounds check, so we keep the
    /// gate at the evaluator boundary instead.
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

    /// Entry point for evaluating the `Context`'s root document.
    ///
    /// This is the single, supported way to start evaluation: it pins
    /// `scope.reference_root` to the same `Arc<Node>` we hand to
    /// [`Self::eval`], so the `is_root` pointer-equality check inside
    /// `eval_internal` actually triggers and `&sibling`/`&root` references
    /// don't have to fall through the lazy-thunk safety net.
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
        // Step budget. Skip the atomic entirely on the unlimited path —
        // hosts that don't set `max_steps` should pay zero cost here.
        if let Some(limit) = self.context.capabilities.max_steps {
            // Relaxed is fine: we never need cross-thread visibility ordering
            // here, only that the counter increases monotonically.
            let prev = self.context.step_counter.fetch_add(1, Ordering::Relaxed);
            if prev >= limit {
                return Err(RuntimeError::StepLimitExceeded {
                    limit,
                    range: node.range,
                });
            }
        }

        let mut current_scope = Arc::clone(scope);

        // Pre-eval pass: every decorator gets a chance to mutate the scope or
        // take over the value. Built-in keywords (`@import`, `@schema`) and
        // host-registered plugins go through the same dispatch.
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
                    // List elements are forced through `force_thunk_with_scope`
                    // which doesn't consult `path` / `cache_key`, so they stay
                    // empty here.
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
                                // Important: overrides existing keys in map
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
            // Wrap pass: each decorator either runs through its registered
            // [`DecoratorPlugin::wrap`] (built-ins, host-registered plugins)
            // or falls back to the closure / native-function lookup below
            // for user-defined decorators that share a dict with their data.
            //
            // Plugins may also override [`DecoratorPlugin::wrap_with_ast`]
            // to consume the raw AST args before evaluation; if that hook
            // returns `Some`, regular `wrap` is skipped for that decorator.
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
                    // Delegate to the shared `brand_string_for` so the
                    // field-level type hint (`Type field: ...`) and the
                    // decorator form (`@brand(Type)`) produce identical
                    // brand strings — generics and `?` are preserved on
                    // both sides.
                    d.brand = crate::builtin_decorators::brand_string_for(type_hint);
                }
            }
        }

        Ok(val)
    }

    /// Apply an `@import(path, as=, spread=)` decorator's effect on the scope.
    ///
    /// Exposed so the [`crate::builtin_decorators::ImportDecorator`] plugin can
    /// reuse the implementation without copy-pasting filesystem and module-cache
    /// logic.
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

    /// Evaluate a decorator/function-call argument list against `scope`,
    /// preserving positional order and named bindings.
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

    /// Resolve and evaluate the module identified by `path_str`, threading it
    /// through the registered [`ModuleResolver`] chain. The first resolver
    /// returning `Some(ModuleSource)` wins; the resulting source is parsed and
    /// evaluated once, then cached by `canonical_id`.
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
        if self
            .context
            .loading_modules
            .lock()
            .unwrap()
            .contains(&source.canonical_id)
        {
            return Err(RuntimeError::CircularImport(
                self.context.loading_modules.lock().unwrap().clone(),
                range.into(),
            ));
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

    /// Construct a tagged-enum variant value: `EnumName.Variant { fields }`.
    /// Looks up the parent enum schema in scope, validates the body against
    /// the variant's field set (if the schema is a sum type), and emits a
    /// `Value::Dict` branded with `variant` and `variant_of = enum_name`.
    pub(crate) fn eval_variant_ctor(
        &self,
        enum_path: &[String],
        variant: &str,
        body: &Node,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        // Resolve the head identifier; everything else is path access.
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
        // Slow-path schema lowering doesn't know the binding name; fall
        // back to the enum_path so the brand metadata is non-empty.
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
        // Build the body dict, then validate field-by-field against the
        // variant's spec. Empty body is fine for unit variants.
        let body_val = self.eval(body, scope)?;
        let Value::Dict(body_dict) = body_val else {
            return Err(RuntimeError::TypeMismatch {
                expected: "Dict variant body".into(),
                found: "non-Dict".into(),
                range,
            });
        };
        // Try to take ownership of the body dict to skip cloning the
        // BTreeMap; falls back to cloning when the Arc is shared (rare,
        // only when the body expression is reachable elsewhere).
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
                return entry.func.call(NativeArgs::from_evaluated(args), range);
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

    /// Sandbox gate for native fns. Legacy `register_fn` entries (`gated == false`)
    /// always pass; gated entries pass when `allow_all_native_fn` is on or the
    /// name is in `allow_native_fn`.
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
            reason: if entry.caps.reads_fs {
                "function declared `reads_fs` but is not in the sandbox allowlist".to_string()
            } else {
                "function not in sandbox allowlist".to_string()
            },
            range,
        })
    }

    /// Fallback path for decorators that aren't registered as
    /// [`DecoratorPlugin`]s. Tries (in order) a user-defined closure, then a
    /// registered native function. Built-in keywords (`@import`, `@schema`,
    /// `@expect`, `@default`, `@value`) are handled via the plugin registry,
    /// not here.
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
                let mut native = NativeArgs::from_evaluated(args);
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
        // Pin the body inside a single Arc so the scope's `reference_root`
        // and the `&Node` we pass to `self.eval` have identical pointer
        // identity. Without this, `is_root` would never trigger for closure
        // bodies and `&sibling` lookups inside the body would silently fall
        // back through the parent chain — which is fine when the parent has
        // the same fields, but incorrect when (for instance) the closure is
        // declared at file root: the outer dict's reference_root_scope would
        // leak in and we'd resolve siblings against the *call site*'s root
        // instead of the body's.
        let body_arc = Arc::new(body.clone());
        let body_scope = Arc::new(Scope {
            parent: Some(Arc::clone(&bindings_scope)),
            path_node: None,
            locals: Mutex::new(HashMap::new()),
            current_dir: bindings_scope.current_dir.clone(),
            cache_namespace: bindings_scope.cache_namespace.clone(),
            reference_root: Some(Arc::clone(&body_arc)),
            reference_root_parent: Some(bindings_scope.clone()),
            // Reset: a fresh `reference_root` invalidates the inherited
            // `reference_root_scope`. The Dict branch in `eval_internal`
            // re-installs it via the (now-honest) `is_root` check.
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

                // Only eager-eval `@schema` whose body is a literal Dict — that
                // form has a fixed, side-effect-free extraction path. Compositional
                // forms (e.g. `&sibling.Base + { ... }`) reference siblings while
                // being evaluated, which would re-enter `prepare_dict_scope` on the
                // same dict and recurse forever. Defer those to lazy thunk eval.
                let is_dict_schema = is_schema && matches!(value_node.expr.as_ref(), Expr::Dict(_));
                // Sum-type Enum schemas (`@schema X: Enum<A {...}, B>`) live in a
                // Type-bodied node. Same eager-eval rationale as the Dict case:
                // sibling refs to `X` need a value before lazy thunks can resolve.
                let is_enum_schema = is_schema
                    && matches!(value_node.expr.as_ref(),
                        Expr::Type(t) if t.path.len() == 1 && t.path[0] == "Enum");

                if Self::is_logic_definition(value_node) || is_dict_schema || is_enum_schema {
                    let key_str = match key {
                        TokenKey::String(s, _, _) => s.clone(),
                        TokenKey::Dynamic(expr_node, _) => match self.eval(expr_node, scope)? {
                            Value::String(s) => s,
                            Value::Int(i) => i.to_string(),
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
                        scope
                            .locals
                            .lock()
                            .unwrap()
                            .insert(key_str.clone(), Value::Schema { generics: Vec::new(), fields: HashMap::new() });
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

/// Module-level helper because both [`Evaluator`] methods and the
/// schema-extraction path need to compute a decorator's lookup key.
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
