use crate::decorator::PreEvalOutcome;
use crate::error::RuntimeError;
use crate::module::{FilesystemModuleResolver, ModuleSource, StdModuleResolver};
use crate::native_fn::{EvaluatedArg, NativeArgs, NativeFnCaps};
use crate::scope::{Scope, Thunk};
use crate::value::Value;
use relon_eval_api::context::{Context, GatedNativeFn};
use relon_parser::{
    is_builtin_type_name, parse_document, CallArg, Decorator as DecoratorNode, Expr, FStringPart,
    Node, Operator, TokenKey, TokenRange,
};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// Tree-walking interpreter. Implements [`relon_eval_api::Evaluator`]
/// and is the default backend hosts construct directly.
///
/// Previously named `Evaluator`; the rename makes room for alternative
/// backends (a bytecode VM is on the roadmap) without ambiguity at the
/// host import site.
pub struct TreeWalkEvaluator {
    pub context: Arc<Context>,
    /// Lazy cache for the `Arc<dyn NativeFnCaps>` handed to native fns so
    /// closures can call back into Relon. Allocating one per call shows up
    /// in the per-element hot path of `_list_map`/`_list_filter` etc.
    caps: std::sync::OnceLock<Arc<EvaluatorCaps>>,
    /// Cached empty scope used as the parent of native-fn closure
    /// callbacks. Avoids one `Arc::new(Scope::default())` per element.
    empty_scope: std::sync::OnceLock<Arc<Scope>>,
}

impl TreeWalkEvaluator {
    /// Build a [`TreeWalkEvaluator`] over `context`.
    ///
    /// The first wrap installs the tree-walking backend's default
    /// stdlib / decorators / prelude (see `prepare_tree_walk_context`).
    /// Subsequent wraps short-circuit because
    /// [`Context::backend_prepared`] stays `true` once flipped.
    ///
    /// Hosts that build the context inline (`Arc::new(ctx)`) get the
    /// historical "Context::new() then go" experience for free. Hosts
    /// that share the `Arc<Context>` (clone it before passing in) MUST
    /// call [`Self::prepare_in_place`] on the bare `Context` before the
    /// first `Arc::new`. Otherwise this constructor panics with a clear
    /// message — silent skip would surface later as
    /// `RuntimeError::FunctionNotFound` for stdlib methods, which is a
    /// nightmare to debug.
    pub fn new(context: Arc<Context>) -> Self {
        let context = prepare_tree_walk_context(context);
        Self {
            context,
            caps: std::sync::OnceLock::new(),
            empty_scope: std::sync::OnceLock::new(),
        }
    }

    /// Install the tree-walking backend's defaults into `ctx`
    /// in place. Hosts that share a single `Arc<Context>` across
    /// multiple evaluators should call this on the bare `Context`
    /// before wrapping, so the wrap-time registration check (which
    /// requires unique `Arc` ownership) is not relied upon.
    pub fn prepare_in_place(ctx: &mut Context) {
        if ctx.backend_prepared {
            return;
        }
        crate::builtin_decorators::register_to(ctx);
        crate::stdlib::register_to(ctx);
        crate::prelude::seed_prelude_schemas(&mut ctx.schemas);
        ctx.module_resolvers.insert(0, Arc::new(StdModuleResolver));
        if ctx.sandboxed_flag {
            ctx.module_resolvers
                .push(Arc::new(FilesystemModuleResolver::default()));
        }
        ctx.backend_prepared = true;
    }

    /// v6-fix-D2-I cold-start lite-mode preparation. Skips every
    /// registration `prepare_in_place` does — no `builtin_decorators`,
    /// no `stdlib`, no `prelude` schema seed, no `StdModuleResolver`
    /// (the trivial scalar `#main` envelope provably touches none of
    /// these). Marks the Context as prepared so the wrap-time check
    /// in `prepare_tree_walk_context` short-circuits.
    ///
    /// **Contract**: caller must guarantee the source under this
    /// Context fits the trivial-scalar `#main` envelope as classified
    /// by `relon::is_trivial_scalar_main` — single
    /// Int/Float/Bool/Null/String parameter, body is a literal /
    /// Variable / arithmetic / ternary over leaves, no `#import`,
    /// no decorator, no fn call. Outside that envelope this skips
    /// registrations the evaluator may dispatch into (stdlib lookup,
    /// decorator pre-eval) and would surface as
    /// `RuntimeError::FunctionNotFound`. The CLI wires this only on
    /// the `lite_analyze` path which has already pinned the entry
    /// shape via the same classifier.
    pub fn prepare_in_place_lite(ctx: &mut Context) {
        if ctx.backend_prepared {
            return;
        }
        // The lite shape never invokes a native fn / decorator, so we
        // intentionally skip every registration here. Flip the flag so
        // `TreeWalkEvaluator::new`'s wrap-time `prepare_tree_walk_context`
        // sees the Context as already prepared and doesn't insist on
        // unique `Arc` ownership for a now-unnecessary re-registration
        // pass.
        ctx.backend_prepared = true;
    }

    pub(crate) fn caps(&self) -> Arc<dyn NativeFnCaps> {
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

    /// Enforce `Capabilities::max_value_elements`. We count elements in
    /// `List` / `Dict` and skip primitive values entirely (their size is
    /// bounded by the source).
    pub(crate) fn check_value_size(
        &self,
        value: &Value,
        range: TokenRange,
    ) -> Result<(), RuntimeError> {
        let Some(limit) = self.context.capabilities.max_value_elements else {
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
        // Reset the step budget so hosts that reuse one `TreeWalkEvaluator` for
        // multiple independent top-level evaluations don't carry over
        // counts from prior runs. Module loads happen *inside* this
        // top-level walk and intentionally do not reset.
        self.context.step_counter.store(0, Ordering::Relaxed);
        // Also clear `path_cache`: cache keys are derived from
        // `scope.path_cache_key(keys)` and don't include `#main` args
        // or any other per-invocation state, so reusing a Context across
        // top-level runs would otherwise hand back a stale value for an
        // identical reference path. `module_cache` is intentionally left
        // alone — module loads are genuinely cross-run shareable.
        // `evaluating_paths` is per single eval cycle and is already
        // managed by the resolver; don't touch it here.
        self.context.path_cache.lock().unwrap().clear();
        // Drop every cursor from the previous top-level run. The
        // matching id counter is *not* reset (see
        // `Context::iter_id_counter` doc) so a still-live `Iter`
        // dict surviving from a prior run can't collide with a
        // fresh one minted on this run.
        self.context.iter_cursors.lock().unwrap().clear();
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
        // Same rationale as `eval_root`: path_cache keys don't include
        // host-pushed `#main` args, so without a clear here a second
        // invocation with different args would return cached values
        // from the first run.
        self.context.path_cache.lock().unwrap().clear();
        // Same rationale as the matching clear in `eval_root`: any
        // cursor entries from the previous top-level run go away
        // here, so a Context reused across `run_main` calls never
        // accumulates iter state.
        self.context.iter_cursors.lock().unwrap().clear();
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

        // v1.8+ fix (issue 1): apply root-level `#import` directives
        // *before* main-arg type-checking so a `#main(pkg.Schema u)`
        // signature can be validated against the imported alias.
        // Pre-fix `apply_directive_pre` for `#import` ran inside the
        // main `eval(root)` call (after args were already type-checked
        // and bound), so `pkg` wasn't in scope when `check_type` tried
        // to resolve the param's type — the analyzer let `lib.User u`
        // through but the runtime errored with `Variable not found:
        // lib`. We mirror the same `apply_directive_pre` walk the
        // evaluator does at the start of `eval()`, but only for
        // root-level directives. Bare `#schema` overrides are
        // surfaced as the entry's value (matching `eval_root`).
        let mut current_scope = scope.clone();
        for dir in &root.directives {
            if let Some(override_val) = self.apply_directive_pre(dir, &root, &mut current_scope)? {
                return Ok(override_val);
            }
        }
        let scope = current_scope;

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
                .locals_for_write()
                .insert(Arc::from(param.name.as_str()), value);
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
            scope.locals_for_write().insert(
                Arc::from(decl.name.as_str()),
                Value::Schema(Box::new(crate::value::SchemaData {
                    // v1.8+ fix (issue 4): the placeholder uses the
                    // real generic param names so a recursive body
                    // referring to `Box<T>` already sees the right
                    // shape during predicate building.
                    generics: decl.generics.clone(),
                    fields: HashMap::new(),
                })),
            );
            let (lowered, _diags) = relon_analyzer::lower_schema_pure(
                Some(decl.name.clone()),
                // v1.8+ fix (issue 4): forward the directive's
                // generic param names so the lowered `SchemaDef`
                // carries them. Pre-fix this passed `Vec::new()`,
                // dropping the generics entirely — `Box<Int>` then
                // had no `T` to substitute against.
                decl.generics.clone(),
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
                Value::Schema(Box::new(crate::value::SchemaData {
                    generics: def.generics.clone(),
                    fields,
                }))
            };
            scope
                .locals_for_write()
                .insert(Arc::from(decl.name.as_str()), value);
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
        Value::EnumSchema(Box::new(crate::value::EnumSchemaData {
            name: def.name.clone().unwrap_or_default(),
            generics: def.generics.clone(),
            variants,
        }))
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
            Expr::String(s) => Ok(Value::String(s.as_str().into())),

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
                    current_dir: current_scope.current_dir.clone(),
                    cache_namespace: current_scope.cache_namespace.clone(),
                    root_ref: current_scope.root_ref.clone(),
                    list_context: current_scope.list_context.clone(),
                    ..Default::default()
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
                                        .locals_for_write()
                                        .insert(Arc::from(k.as_str()), v.clone());
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
                                        Value::String(s) => s.into_string(),
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
                            dict_scope
                                .locals_for_write()
                                .insert(Arc::from(key_str), val);
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
                // Decision 21 (Iterable lowering): drive iteration over
                // any value the evaluator can convert into a sequence of
                // elements. The branch order is intentional —
                //   1. `List` is the legacy fast path (most common,
                //      avoids the Iter-wrapping detour).
                //   2. `Iter`-branded `Dict` is the new path that opens
                //      `for x in c` to user schemas that derive
                //      `Iterable` and return `c.iter()` from their
                //      witness. Built-in `List.iter()` / `String.iter()`
                //      / `Dict.iter()` produce values of this shape too
                //      so user iteration over primitives is uniform.
                // Anything else is an error — same diagnostic shape as
                // before, just with an updated `expected` slot.
                let items = self.materialize_iterable(&iter_val, iterable.range)?;
                // Pre-size the result. Without a filter, length is
                // exact; with a filter, `items.len()` is an upper
                // bound and over-allocating is still cheaper than the
                // ~log2(n) doubling steps a `Vec::new()` would incur
                // (the eight grow steps on a 1000-elem comprehension
                // were the second-biggest line item in the dhat
                // attribution table).
                let mut result: Vec<Value> = Vec::with_capacity(items.len());
                // Intern the loop variable name once: each iteration
                // rebinds it under the same outer scope, so without this
                // hoist we were paying one `String::clone` (malloc +
                // memcpy) per element. After interning, the inner loop
                // only bumps an `Arc` refcount per element.
                let id_arc: Arc<str> = Arc::from(id.as_str());
                // Build the iter-loop frame ONCE outside the body loop.
                // Previously each iteration called `with_local`, which
                // allocated a fresh `Arc<Scope>` per element — the
                // 48 MB / 200 K-block hot site flagged by P1-B's
                // diagnostic correction. `with_iter_loop` reuses one
                // frame; `set_iter_binding` refreshes the binding via a
                // single Mutex peek + Value clone per element. Closure
                // construction inside the body snapshots the binding
                // via `Scope::current_iter_binding` (handled in the
                // `Expr::Closure` branch) so lexical-capture semantics
                // hold even though the frame is mutated.
                //
                // `materialize_iterable` returns a `Cow` borrowed from
                // the input list when possible; we still need an
                // `elements` vec of thunks for the `&prev` / `&next`
                // pathway. For comprehensions over an `Iter`-branded
                // value the thunks aren't actually used today —
                // `&prev` / `&next` only fire inside list literals —
                // so an empty `elements` vec is the cheapest stand-in
                // and keeps the API uniform.
                let outer_scope = current_scope.with_iter_loop(Vec::new());
                for (i, item) in items.iter().enumerate() {
                    outer_scope.set_iter_binding(Arc::clone(&id_arc), item.clone(), i);

                    let should_include = if let Some(cond) = condition {
                        self.eval(cond, &outer_scope)?.is_truthy()
                    } else {
                        true
                    };
                    if should_include {
                        result.push(self.eval(element, &outer_scope)?);
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
                let base_env = if scope.path_node.is_some() {
                    scope.parent.clone().unwrap_or_else(|| Arc::clone(scope))
                } else {
                    Arc::clone(scope)
                };
                // Lexical-capture safety: when the closure is constructed
                // inside a comprehension hot loop, the visible `for x in
                // xs` binding lives in a shared, mutable `iter_binding`
                // slot on `list_context`. If we captured the outer scope
                // by `Arc` only, the *next* iteration would clobber the
                // value the closure was meant to remember. Snapshot it
                // into a plain `with_local` child so the closure sees the
                // bound value the loop body saw at construction time.
                //
                // Also walk up the parent chain — nested comprehensions
                // (`[[y for y in ys] for x in xs]`) park each `for` on
                // its own `list_context`, and outer bindings need
                // snapshotting too.
                let captured_env = snapshot_iter_bindings(&base_env);

                Ok(Value::Closure(Box::new(crate::value::ClosureData {
                    params: param_names,
                    body: body.clone(),
                    captured_env,
                })))
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
                        if let Value::Closure(closure) = right_val {
                            let crate::value::ClosureData {
                                params,
                                body,
                                captured_env,
                            } = *closure;
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
                    let map_as_hashmap: std::collections::HashMap<Arc<str>, Value> = d
                        .map
                        .iter()
                        .map(|(k, v)| (Arc::from(k.as_str()), v.clone()))
                        .collect();
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
                use std::fmt::Write as _;
                let mut result = String::new();
                for part in parts {
                    match part {
                        FStringPart::Literal(s) => result.push_str(s),
                        FStringPart::Interpolation(node) => {
                            let val = self.eval(node, &current_scope)?;
                            let _ = write!(result, "{}", val);
                        }
                    }
                }
                Ok(Value::String(result.into()))
            }
            Expr::Type(t) => Ok(Value::Type(Box::new(t.clone()))),
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
        generics: &[String],
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
                return Ok(Value::Schema(Box::new(crate::value::SchemaData {
                    generics: def.generics.clone(),
                    fields,
                })));
            }
        }
        let (lowered, _diags) =
            relon_analyzer::lower_schema_pure(Some(name.to_string()), generics.to_vec(), body);
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
        Ok(Value::Schema(Box::new(crate::value::SchemaData {
            generics: def.generics.clone(),
            fields,
        })))
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
                if let DirectiveBody::Import {
                    spec,
                    path,
                    integrity,
                    ..
                } = &directive.body
                {
                    // review-improvement-174: forward the inline integrity
                    // pin (if any) so the evaluator's import path verifies
                    // the loaded module body. Without this, a host that
                    // skips the analyzer's workspace pass — for example a
                    // bench harness that parses straight into the
                    // evaluator — would silently load a poisoned remote
                    // module even though its `sha256:"..."` pin disagreed.
                    let new_scope = self.apply_directive_import(
                        spec,
                        path,
                        integrity.as_ref(),
                        current_scope,
                        directive.range,
                    )?;
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
                            return Ok(Some(Value::Schema(Box::new(crate::value::SchemaData {
                                generics: def.generics.clone(),
                                fields,
                            }))));
                        }
                    }
                    let (lowered, _diags) =
                        relon_analyzer::lower_schema_pure(None, Vec::new(), node);
                    if let Some(def) = lowered {
                        if !def.variants.is_empty() {
                            return Ok(Some(self.build_root_enum_schema(&def)));
                        }
                        let fields = self.build_schema_from_def(&def, current_scope)?;
                        return Ok(Some(Value::Schema(Box::new(crate::value::SchemaData {
                            generics: def.generics.clone(),
                            fields,
                        }))));
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
    ///
    /// `integrity`, when present, is the inline `sha256:"..."` pin
    /// parsed off the directive. The evaluator verifies the loaded
    /// module body against it before exposing any bindings — see the
    /// `verify_module_integrity` helper for the algorithm /
    /// fail-closed semantics. Passing `None` mirrors the previous
    /// behaviour for unpinned imports (local paths, std modules, hosts
    /// that opt out of pinning).
    pub fn apply_directive_import(
        &self,
        spec: &relon_parser::DirectiveImportSpec,
        path: &str,
        integrity: Option<&relon_parser::IntegrityHash>,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Arc<Scope>, RuntimeError> {
        use relon_parser::DirectiveImportSpec;
        let evaluated_module = self.load_module(path, integrity, scope, range)?;
        let mut new_locals: HashMap<Arc<str>, Value> = HashMap::new();
        match spec {
            DirectiveImportSpec::Alias(name) => {
                new_locals.insert(Arc::from(name.as_str()), evaluated_module);
            }
            DirectiveImportSpec::Spread => {
                if let Value::Dict(d) = evaluated_module {
                    for (k, v) in d.map.iter() {
                        new_locals.insert(Arc::from(k.as_str()), v.clone());
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
                    let local_name: Arc<str> = match alias {
                        Some(a) => Arc::from(a.as_str()),
                        None => Arc::from(name.as_str()),
                    };
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
    /// vector a [`crate::decorator::DecoratorPlugin::schema_field_meta`] hook expects.
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

    /// Resolve `#import "path"` against the registered resolver chain
    /// and evaluate the resulting source. Resolvers are tried in order;
    /// the first one returning `Some(ModuleSource)` wins. Resolved
    /// modules are evaluated with their own `current_dir` so nested
    /// imports inside the module are anchored to the module's location,
    /// not the host's.
    ///
    /// When `integrity` is `Some`, the loaded body's digest is verified
    /// against the inline pin *before* the module body is parsed or
    /// evaluated. Mismatch / unsupported-algo / malformed-hex cases all
    /// produce a typed [`RuntimeError`] and the module body never
    /// reaches the evaluator's parser — fail-closed by design so a
    /// poisoned remote source cannot influence anything downstream
    /// (cache, scope, side-effects). See `verify_module_integrity` for
    /// the per-branch decisions.
    pub fn load_module(
        &self,
        path_str: &str,
        integrity: Option<&relon_parser::IntegrityHash>,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let source = self.resolve_module_source(path_str, scope, range)?;
        // review-improvement-174: enforce the inline pin *after*
        // resolution (so we have the actual bytes) but *before* parse /
        // cache / evaluation, so an attacker-controlled body that
        // disagrees with its pin is dropped with zero side effects.
        if let Some(pin) = integrity {
            verify_module_integrity(path_str, pin, source.source.as_bytes(), range)?;
        }
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

        // Fast path: workspace pre-analyzed this module, so we can
        // pull both the parsed root node and the analyzer's verdict
        // out of the workspace tree directly. The workspace pass is
        // also where structural / cycle / not-found errors are now
        // raised — by the time we reach the evaluator, the entry has
        // already passed `WorkspaceTree::has_errors`. So an unexpected
        // missing-from-workspace module here is a bug in the host
        // (workspace was assembled from a different entry) rather
        // than a user-reachable error; we fall back to parse+analyze
        // on the spot to keep behavior conservative.
        let node_arc: Arc<Node> = if let Some(ws) = &self.context.workspace {
            if let Some(arc) = ws.nodes.get(&source.canonical_id) {
                Arc::clone(arc)
            } else {
                fallback_parse_analyze(&source, range)?
            }
        } else {
            fallback_parse_analyze(&source, range)?
        };
        let module_scope = Arc::new(Scope {
            current_dir: source.current_dir,
            cache_namespace: source.canonical_id.clone(),
            root_ref: Some(crate::scope::RootRef::new(Arc::clone(&node_arc))),
            ..Default::default()
        });
        let mut evaluated = self.eval(&node_arc, &module_scope)?;
        // v1.8+ fix (issue 1): expose the lib's root-level `#schema X
        // { ... }` declarations as fields on the evaluated module so
        // `#main(lib.X u)` (alias-form import) can resolve `X`
        // through the module's value at type-check time. Pre-fix the
        // module value was just the dict body — `lib.User` failed
        // with `Variable not found: lib.User` even after `lib` was
        // bound, because `lib` had no `User` field. We only inject
        // when the body is itself a Dict; non-dict module bodies
        // (atomic root, list, ...) don't have a natural place to
        // hang named schemas, so cross-module schema reference
        // through them stays unsupported.
        if let Value::Dict(ref mut d) = evaluated {
            if let Some(ws) = &self.context.workspace {
                if let Some(tree) = ws.modules.get(&source.canonical_id) {
                    if !tree.root_schemas.is_empty() {
                        // Build each schema value the same way
                        // `seed_root_schemas` does, then merge into
                        // the dict map. Existing dict fields win on
                        // collision (the user's data takes
                        // precedence over the schema name).
                        let d_mut = Arc::make_mut(d);
                        for decl in &tree.root_schemas {
                            if d_mut.map.contains_key(&decl.name) {
                                continue;
                            }
                            let (lowered, _diags) = relon_analyzer::lower_schema_pure(
                                Some(decl.name.clone()),
                                // v1.8+ fix (issue 4): forward the
                                // generics so a `lib.Box<Int>` lookup
                                // can substitute T → Int through the
                                // module-injected `Value::Schema`.
                                decl.generics.clone(),
                                decl.schema_node.as_ref(),
                            );
                            let Some(def) = lowered else { continue };
                            let value = if !def.variants.is_empty() {
                                self.build_root_enum_schema(&def)
                            } else {
                                let fields = self.build_schema_from_def(&def, &module_scope)?;
                                Value::Schema(Box::new(crate::value::SchemaData {
                                    generics: def.generics.clone(),
                                    fields,
                                }))
                            };
                            d_mut.map.insert(decl.name.clone(), value);
                        }
                    }
                }
            }
        }
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
        // Resolve the head identifier through the local scope first, then
        // fall back to the context's schema table. The latter is what
        // makes prelude entries (`Result`, `Option`) reachable as
        // `Result.Ok { ... }` without a user-side `#schema` declaration.
        let mut current = scope
            .get_local(head)
            .or_else(|| self.context.schemas.get(head).cloned())
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
        let Value::EnumSchema(enum_box) = current else {
            return Err(RuntimeError::TypeMismatch {
                expected: format!("EnumSchema `{enum_name}`"),
                found: current.type_name().to_string(),
                range,
            });
        };
        let crate::value::EnumSchemaData {
            name,
            generics,
            variants,
        } = *enum_box;
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
                // Skip the type check when the declared field type is
                // a bare reference to one of the enum's generic
                // parameters (e.g. `value: T` inside `Result<T, E>`).
                // The substitution is supplied at the use site by the
                // surrounding `check_type` (via `Result<Int, String>`
                // → `T -> Int`), so demanding the bare `T` resolve in
                // *this* scope would always fail. Concrete (non-type-
                // variable) field types are still validated here.
                if !is_type_variable(&field_def.type_hint, &generics) {
                    self.check_type(fval, &field_def.type_hint, scope, range)?;
                }
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
            Value::Closure(closure) => {
                let crate::value::ClosureData {
                    params,
                    body,
                    captured_env,
                } = *closure;
                self.eval_closure(&params, &body, args, &captured_env, range)
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
        if let Ok(Value::Closure(closure)) = self.resolve_variable(path, scope, range) {
            let crate::value::ClosureData {
                params,
                body,
                captured_env,
            } = *closure;
            return self.eval_closure(&params, &body, args, &captured_env, range);
        }
        if let Some(name) = Self::native_function_name(path) {
            if let Some(entry) = self.context.functions.get(&name) {
                self.check_native_fn_capability(&name, entry, range)?;
                let result = entry
                    .func
                    .call(NativeArgs::from_evaluated(args, self.caps()), range)?;
                // Catch-all enforcement of `max_value_elements` for
                // every `List` / `Dict` produced by a native fn —
                // covers `range`, `string.split`, `dict.merge` (free
                // form), `_list_map` / `_list_filter`, the `iter()`
                // family, host-registered native fns, etc., without
                // sprinkling a per-intrinsic check. `check_value_size`
                // only inspects the outermost container, so wrappers
                // like the `Iter`-branded dict (`{ _kind, _source,
                // _id }`) are sized as a 3-key dict regardless of how
                // large `_source` is — exactly the desired semantics.
                self.check_value_size(&result, range)?;
                return Ok(result);
            }
        }
        // Schema-rooted Phase B: dispatch `value.method(...)` and
        // `Schema.method(...)` by consulting the analyzed tree's
        // `schema_methods` table. The lookup is keyed by the schema
        // name extracted from either the receiver value's brand /
        // primitive tag, or the schema name itself for static calls.
        if let Some(result) = self.try_call_schema_method(path, &args, scope, range)? {
            return Ok(result);
        }
        Err(RuntimeError::FunctionNotFound(
            path.iter()
                .map(|k| k.to_string_key())
                .collect::<Vec<_>>()
                .join("."),
            range,
        ))
    }

    /// Dispatch an `Expr::FnCall` whose path looks like
    /// `[receiver_root, ..fields, method]` (`path.len() >= 2`) against
    /// the analyzer's `schema_methods` table. The receiver value is
    /// resolved by walking `path[..-1]`, so chained access like
    /// `o.customer.greet()` lands on `User.greet` once `o.customer`'s
    /// runtime value carries a `"User"` brand. Returns:
    ///
    ///   * `Ok(Some(value))` — dispatched and evaluated successfully.
    ///   * `Ok(None)` — not a recognizable method call (so the caller
    ///     should fall through to its own error path).
    ///   * `Err(_)` — the call dispatched but evaluating the body
    ///     failed.
    fn try_call_schema_method(
        &self,
        path: &[TokenKey],
        args: &[EvaluatedArg],
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Option<Value>, RuntimeError> {
        if path.len() < 2 {
            return Ok(None);
        }
        let last_idx = path.len() - 1;
        let TokenKey::String(method_name, _, _) = &path[last_idx] else {
            return Ok(None);
        };
        let Some(analyzed) = self.context.analyzed.as_ref() else {
            return Ok(None);
        };
        // Static dispatch first: head names a schema directly, and
        // the path is exactly `Schema.method` (2 segments). Multi-hop
        // paths can't be static — `Order.User.greet` doesn't make
        // sense; the prefix must resolve to a value at runtime.
        if path.len() == 2 {
            if let TokenKey::String(head, _, _) = &path[0] {
                if let Some(methods) = analyzed.schema_methods.get(head) {
                    if let Some(method) = methods.iter().find(|m| m.name == *method_name) {
                        // Phase D: a `#native` method declared at source
                        // level with no body delegates to a host-registered
                        // impl. Try that table before falling through to
                        // any (currently un-supported) static-body
                        // dispatch.
                        if method.is_native {
                            if let Some(out) =
                                self.try_call_native_method(head, method_name, None, args, range)?
                            {
                                return Ok(Some(out));
                            }
                        }
                        if let Some(body) = method.body_node.as_ref() {
                            return self
                                .invoke_method_body(body, None, &method.params, args, scope, range)
                                .map(Some);
                        }
                    }
                }
            }
        }
        // v6-δ M1 R4: receiver dispatch for `<expr>.method(...)` where
        // the receiver is an expression (string literal, list literal,
        // parenthesised expr, etc.) — the parser lowers these as
        // `TokenKey::Dynamic(inner_node, ...)`. We evaluate the inner
        // node directly and use its value's brand / primitive tag as
        // the schema name. Without this branch
        // `"hello".length()` / `[1,2,3].sum()` were both
        // FunctionNotFound because `resolve_variable` only walks the
        // scope chain (string literals never make it into a local).
        let receiver_value = if path.len() == 2 {
            if let TokenKey::Dynamic(inner, _) = &path[0] {
                self.eval(inner, scope)?
            } else {
                match self.resolve_variable(&path[..last_idx], scope, range) {
                    Ok(v) => v,
                    Err(_) => return Ok(None),
                }
            }
        } else {
            // Receiver dispatch: walk `path[..-1]` to materialize the
            // receiver value, then read its schema tag. For 2-segment
            // calls (`m.method`) this collapses to the original
            // single-name lookup; for 3+ segments (`o.customer.method`)
            // we descend through the intermediate fields using the same
            // `resolve_variable` driver that powers `Expr::Variable`.
            let prefix = &path[..last_idx];
            match self.resolve_variable(prefix, scope, range) {
                Ok(v) => v,
                Err(_) => return Ok(None),
            }
        };
        let Some(schema_name) = value_schema_tag(&receiver_value) else {
            return Ok(None);
        };
        // Phase D: receiver-side native method dispatch. Try the
        // host-registered table first so schemas may attach behaviors
        // even when the source-side `with { ... }` block is empty
        // (e.g. body-less `#schema String with { ... }` from
        // `register_pure_method("String", "is_blank", ...)`).
        if let Some(out) = self.try_call_native_method(
            &schema_name,
            method_name,
            Some(receiver_value.clone()),
            args,
            range,
        )? {
            return Ok(Some(out));
        }
        let Some(methods) = analyzed.schema_methods.get(&schema_name) else {
            return Ok(None);
        };
        let Some(method) = methods.iter().find(|m| m.name == *method_name) else {
            return Ok(None);
        };
        let Some(body) = method.body_node.as_ref() else {
            return Ok(None);
        };
        self.invoke_method_body(
            body,
            Some(receiver_value),
            &method.params,
            args,
            scope,
            range,
        )
        .map(Some)
    }

    /// Phase C (Indexable lowering, decision 22): dispatch `a[i]` on a
    /// branded value whose schema derives `Indexable` (witness
    /// `index(key: K) -> Optional<V>`). Returns:
    ///
    ///   * `Ok(Some(value))` — the receiver has an `index` method
    ///     (user body or host-registered native); we dispatched it and
    ///     unwrapped the returned `Optional<V>` per the dynamic key's
    ///     `?` flag:
    ///       - `Some { value: v }` → `v`.
    ///       - `None` → `Value::Null` when `is_optional`; otherwise a
    ///         `VariableNotFound` matching the built-in dict / list
    ///         miss diagnostic.
    ///       - Anything else (non-Option-shaped return) is surfaced
    ///         as-is; the user's witness signature is the contract.
    ///   * `Ok(None)` — no `index` method registered for this value's
    ///     schema tag. Caller falls back to its built-in dict / list
    ///     lookup so plain `dict["foo"]` / `list[0]` keep working.
    ///   * `Err(_)` — the dispatch fired but evaluating the body or
    ///     a sub-step failed.
    ///
    /// `display_name` is used only for the not-found diagnostic when an
    /// `index()` call returns `Option.None` without the `?` flag — it
    /// mirrors the `VariableNotFound` text that the surrounding caller
    /// would have produced via plain key miss.
    pub(crate) fn try_index_method(
        &self,
        receiver: &Value,
        key_value: Value,
        is_optional: bool,
        display_name: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(schema_name) = value_schema_tag(receiver) else {
            return Ok(None);
        };
        let Some(analyzed) = self.context.analyzed.as_ref() else {
            return Ok(None);
        };
        let has_method = analyzed
            .schema_methods
            .get(&schema_name)
            .map(|methods| methods.iter().any(|m| m.name == "index"))
            .unwrap_or(false);
        let has_native = self
            .context
            .native_methods
            .contains_key(&(schema_name.clone(), "index".to_string()));
        if !has_method && !has_native {
            return Ok(None);
        }
        let args = vec![EvaluatedArg::positional(key_value)];
        // Host-registered native impl wins when both exist (mirrors
        // `try_call_schema_method`'s receiver-side native check).
        let raw = if has_native {
            self.try_call_native_method(
                &schema_name,
                "index",
                Some(receiver.clone()),
                &args,
                range,
            )?
            .expect("native method existence checked above")
        } else {
            let methods = analyzed.schema_methods.get(&schema_name).unwrap();
            let method = methods.iter().find(|m| m.name == "index").unwrap();
            if let Some(body) = method.body_node.as_ref() {
                self.invoke_method_body(
                    body,
                    Some(receiver.clone()),
                    &method.params,
                    &args,
                    scope,
                    range,
                )?
            } else {
                // Method recorded without a body (e.g. `#native`
                // declaration whose host registration is missing).
                // Fall through so the caller's diagnostic wins.
                return Ok(None);
            }
        };
        Ok(Some(unwrap_optional_for_index(
            raw,
            is_optional,
            display_name,
            range,
        )?))
    }

    /// Phase D: dispatch through a host-registered native method.
    /// Returns `Ok(None)` when no entry matches `(schema, method)`.
    /// `receiver` is prepended to the positional args when present
    /// (the host fn sees `self` as `args[0]`); static calls pass
    /// `None` and the host fn just sees the declared params.
    fn try_call_native_method(
        &self,
        schema: &str,
        method: &str,
        receiver: Option<Value>,
        args: &[EvaluatedArg],
        range: TokenRange,
    ) -> Result<Option<Value>, RuntimeError> {
        let key = (schema.to_string(), method.to_string());
        let Some(entry) = self.context.native_methods.get(&key) else {
            return Ok(None);
        };
        let display_name = format!("{schema}.{method}");
        self.check_native_fn_capability(&display_name, entry, range)?;
        let mut native = NativeArgs::from_evaluated(args.to_vec(), self.caps());
        if let Some(self_val) = receiver {
            native.positional.insert(0, self_val);
        }
        let result = entry.func.call(native, range)?;
        // Catch-all enforcement of `max_value_elements` for the
        // receiver-side method dispatch path (`xs.map(...)`,
        // `d.merge(other)`, `s.split(...)`, …). Mirrors the post-call
        // check in `call_function`. `check_value_size` looks at the
        // outermost container only, so wrapper dicts like the
        // `Iter`-branded `{ _kind, _source, _id }` produced by
        // `xs.iter()` are sized as a 3-key dict — they don't trip the
        // cap based on the wrapped `_source`'s element count.
        self.check_value_size(&result, range)?;
        Ok(Some(result))
    }

    /// Decision 21 (Iterable lowering): turn an arbitrary iterable
    /// `Value` into the linear element sequence consumed by the
    /// `Expr::Comprehension` driver.
    ///
    /// Recognized shapes:
    ///
    ///   * `Value::List` — fast path, element-by-element.
    ///   * `Value::Dict` with brand `"Iter"` — the wrapped form
    ///     produced by `List.iter()` / `String.iter()` /
    ///     `Dict.iter()` (and any user `Iterable` witness that
    ///     delegates to one of those). Unwrapped using the `_kind`
    ///     tag to dispatch the right driver:
    ///       - `"list"` → element-by-element over the wrapped list.
    ///       - `"string"` → one-codepoint-per-step over the wrapped
    ///         string (each element a fresh single-char `String`).
    ///       - `"dict_entries"` → key-sorted `(K, V)` pairs encoded
    ///         as 2-element `Value::list([k, v])` (the runtime has no
    ///         dedicated tuple variant).
    ///
    /// Any other shape — including raw `String` / `Dict` not first
    /// turned into an iterator — surfaces a `TypeMismatch` whose
    /// `expected` slot now reads "List or Iter" so the user can wire
    /// in the missing `.iter()` call.
    fn materialize_iterable<'a>(
        &self,
        value: &'a Value,
        range: TokenRange,
    ) -> Result<Cow<'a, [Value]>, RuntimeError> {
        // Fast path: a literal `[1, 2, 3]` / `xs` of type `List<T>`
        // lands here without ever being wrapped in `Iter`. Return a
        // borrowed slice so the comprehension loop avoids cloning the
        // whole backing `Vec<Value>` — the loop already does its own
        // per-item `clone()` when binding the iteration variable into
        // the scope, so the intermediate `Vec` was pure waste. dhat
        // attribution flagged this site as the dominant allocator in
        // the `stdlib::Range` / comprehension bucket.
        if let Value::List(items) = value {
            return Ok(Cow::Borrowed(items.as_slice()));
        }
        // Decision 21' Iter representation: branded dict with
        // `_kind` + `_source` fields. We deliberately recurse through
        // the driver here (rather than reading `_source` once and
        // delegating to the surrounding match) so a user-built
        // `Iter`-shaped dict that wraps another `Iter` still works.
        if let Value::Dict(d) = value {
            use crate::iter_protocol::{BRAND, FIELD_KIND, FIELD_SOURCE};
            use crate::iter_protocol::{KIND_DICT_ENTRIES, KIND_LIST, KIND_STRING};
            if d.brand.as_deref() == Some(BRAND) {
                let kind = d
                    .map
                    .get(FIELD_KIND)
                    .and_then(|v| match v {
                        Value::String(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .ok_or_else(|| RuntimeError::TypeMismatch {
                        expected: "Iter with `_kind` String field".to_string(),
                        found: "Iter without `_kind`".to_string(),
                        range,
                    })?;
                let source = d
                    .map
                    .get(FIELD_SOURCE)
                    .ok_or_else(|| RuntimeError::TypeMismatch {
                        expected: "Iter with `_source` field".to_string(),
                        found: "Iter without `_source`".to_string(),
                        range,
                    })?;
                return match kind {
                    KIND_LIST => {
                        let items = match source {
                            Value::List(l) => l,
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "List source for Iter(kind=list)".to_string(),
                                    found: other.type_name().to_string(),
                                    range,
                                })
                            }
                        };
                        // Same zero-clone borrow as the top-level
                        // `Value::List` fast path — works because
                        // `items` is an `Arc<Vec<Value>>` owned by the
                        // input `value`, whose lifetime `'a` outlives
                        // the returned `Cow`.
                        Ok(Cow::Borrowed(items.as_slice()))
                    }
                    KIND_STRING => {
                        let s = match source {
                            Value::String(s) => s,
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "String source for Iter(kind=string)".to_string(),
                                    found: other.type_name().to_string(),
                                    range,
                                })
                            }
                        };
                        Ok(Cow::Owned(
                            s.chars()
                                .map(|c| Value::String(c.to_string().into()))
                                .collect(),
                        ))
                    }
                    KIND_DICT_ENTRIES => {
                        let src_dict = match source {
                            Value::Dict(d) => d,
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "Dict source for Iter(kind=dict_entries)".to_string(),
                                    found: other.type_name().to_string(),
                                    range,
                                })
                            }
                        };
                        // BTreeMap iteration is already key-sorted, so
                        // no explicit sort needed — order matches
                        // `Dict.keys()` / `Dict.values()`.
                        Ok(Cow::Owned(
                            src_dict
                                .map
                                .iter()
                                .map(|(k, v)| {
                                    Value::list(vec![Value::String(k.as_str().into()), v.clone()])
                                })
                                .collect(),
                        ))
                    }
                    other => Err(RuntimeError::TypeMismatch {
                        expected: "Iter._kind in {list, string, dict_entries}".to_string(),
                        found: other.to_string(),
                        range,
                    }),
                };
            }
        }
        Err(RuntimeError::TypeMismatch {
            expected: "List or Iter".to_string(),
            found: value.type_name().to_string(),
            range,
        })
    }

    /// Evaluate a method body with `self` bound (when a receiver is
    /// supplied) plus the positional argument bindings — `self` is
    /// implicit, so positional args map directly onto the declared
    /// param list. Named args fall back to positional ordering for v1;
    /// the analyzer side already validated arity, so a missing arg is
    /// surfaced through the body's own VariableNotFound diagnostics.
    pub(crate) fn invoke_method_body(
        &self,
        body: &Node,
        receiver: Option<Value>,
        params: &[relon_analyzer::schema::SchemaMethodParamInfo],
        args: &[EvaluatedArg],
        scope: &Arc<Scope>,
        _range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let mut bindings: HashMap<Arc<str>, Value> = HashMap::new();
        if let Some(self_val) = receiver {
            bindings.insert(Arc::from("self"), self_val);
        }
        // Positional binding: skip `self` (which is implicit), so the
        // i-th positional arg lands on `params[i].name`.
        let mut pos_idx = 0;
        for arg in args {
            if arg.name.is_some() {
                continue;
            }
            if pos_idx < params.len() {
                bindings.insert(Arc::from(params[pos_idx].name.as_str()), arg.value.clone());
                pos_idx += 1;
            }
        }
        // Named args win over positions when both exist (mirrors
        // `eval_closure`).
        for arg in args {
            if let Some(name) = &arg.name {
                bindings.insert(Arc::from(name.as_str()), arg.value.clone());
            }
        }
        let method_scope = scope.with_locals(bindings);
        self.eval(body, &method_scope)
    }

    /// Tree-walker dispatch-time capability check. Delegates to the
    /// crate-wide [`relon_eval_api::CapabilityGate`] policy boundary
    /// so the cranelift backend (which consults the same trait at
    /// vtable-build time) and any host-supplied gate share a single
    /// source of truth. See `relon_eval_api::capability` for the
    /// enforcement-timing diff between the two backends.
    pub(crate) fn check_native_fn_capability(
        &self,
        name: &str,
        entry: &GatedNativeFn,
        range: TokenRange,
    ) -> Result<(), RuntimeError> {
        use relon_eval_api::CapabilityGate;
        match self.context.capabilities.check_gate(&entry.gate) {
            Ok(()) => Ok(()),
            Err(err) => Err(RuntimeError::CapabilityDenied {
                name: name.to_string(),
                reason: err.reason.label(err.cap),
                range,
            }),
        }
    }

    fn fallback_decorator(
        &self,
        path: &[TokenKey],
        value: Value,
        args: Vec<EvaluatedArg>,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if let Ok(Value::Closure(closure)) = self.resolve_variable(path, scope, range) {
            let crate::value::ClosureData {
                params,
                body,
                captured_env,
            } = *closure;
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
        let mut bindings: HashMap<Arc<str>, Value> = HashMap::new();
        let mut pos_idx = 0;
        for arg in &args {
            if arg.name.is_none() {
                if pos_idx < params.len() {
                    bindings.insert(Arc::from(params[pos_idx].as_str()), arg.value.clone());
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
                if bindings.contains_key(name.as_str()) {
                    return Err(RuntimeError::UnsupportedOperator(
                        format!("Duplicate {}", name),
                        range,
                    ));
                }
                bindings.insert(Arc::from(name.as_str()), arg.value.clone());
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
            locals: Mutex::new(bindings),
            current_dir: captured_env.current_dir.clone(),
            cache_namespace: call_namespace,
            root_ref: captured_env.root_ref.clone(),
            ..Default::default()
        });
        let body_arc = Arc::new(body.clone());
        let body_scope = Arc::new(Scope {
            parent: Some(Arc::clone(&bindings_scope)),
            current_dir: bindings_scope.current_dir.clone(),
            cache_namespace: bindings_scope.cache_namespace.clone(),
            root_ref: Some(crate::scope::RootRef {
                node: Arc::clone(&body_arc),
                scope: None,
                parent_fallback: Some(bindings_scope.clone()),
            }),
            ..Default::default()
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
                let mut locals = scope.locals_for_write();
                for dir in &node.directives {
                    if dir.name != crate::decorator_names::SCHEMA {
                        continue;
                    }
                    let relon_parser::DirectiveBody::NameBody { name, generics, .. } = &dir.body
                    else {
                        continue;
                    };
                    if locals.contains_key(name.as_str()) {
                        continue;
                    }
                    locals.insert(
                        Arc::from(name.as_str()),
                        Value::Schema(Box::new(crate::value::SchemaData {
                            generics: generics.clone(),
                            fields: HashMap::new(),
                        })),
                    );
                    seeded.insert(name);
                }
            }
            for dir in &node.directives {
                if dir.name != crate::decorator_names::SCHEMA {
                    continue;
                }
                let relon_parser::DirectiveBody::NameBody {
                    name,
                    generics,
                    body,
                    ..
                } = &dir.body
                else {
                    continue;
                };
                if !seeded.contains(name.as_str()) {
                    continue;
                }
                let val = self.lower_schema_binding(name, generics, body, scope)?;
                scope
                    .locals_for_write()
                    .insert(Arc::from(name.as_str()), val);
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
                            Value::String(s) => s.into_string(),
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
                        scope.locals_for_write().insert(
                            Arc::from(key_str.as_str()),
                            Value::Schema(Box::new(crate::value::SchemaData {
                                generics,
                                fields: HashMap::new(),
                            })),
                        );
                    }
                    let val = self.eval(value_node, scope)?;
                    scope.locals_for_write().insert(Arc::from(key_str), val);
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
                    Value::String(s) => s.into_string(),
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

        let mut thunks = scope.thunks_for_write();
        for (key_str, value_node) in entries {
            let item_scope = scope.with_path(key_str.clone());
            let path = item_scope.full_path();
            let cache_key = item_scope.path_cache_key(&path);
            thunks.insert(
                Arc::from(key_str),
                Arc::new(Thunk::new(value_node, item_scope, path, cache_key)),
            );
        }
        Ok(())
    }
}

/// Slow path used by `evaluate_module_source` when no workspace tree
/// is wired into the context (or the workspace is missing the module
/// the resolver returned). Parses + analyzes on the spot, mirroring
/// the pre-Stage-0 behavior so single-file consumers (tests that
/// build a `Context` directly without a workspace, ad-hoc embeddings)
/// keep working.
/// Walk the parent chain collecting every active comprehension iter
/// binding, then materialize them into a `with_local`-style snapshot
/// scope. Returns `base` unchanged when no iter bindings are visible
/// — the common "closure defined outside any for-loop" case stays a
/// pure `Arc::clone`, no allocation.
///
/// Why per-walk: nested comprehensions (`[[y for y in ys] for x in xs]`)
/// place each `for` on its own `list_context`. A closure constructed in
/// the inner body needs to remember both `x` and `y` — only the inner
/// `list_context` is reachable via the most recent scope, so we have to
/// walk up to find outer bindings parked on ancestor scopes.
fn snapshot_iter_bindings(base: &Arc<Scope>) -> Arc<Scope> {
    let mut snapshot: Option<HashMap<Arc<str>, Value>> = None;
    let mut visited: std::collections::HashSet<*const Scope> = std::collections::HashSet::new();
    let mut current = Some(base.clone());
    while let Some(scope) = current {
        // Guard against cycles: parent chains in this codebase are
        // tree-shaped, but `RootRef::parent_fallback` can introduce a
        // cross-link in closure-call scopes. A visited-set is the
        // cheapest belt-and-suspenders.
        if !visited.insert(Arc::as_ptr(&scope)) {
            break;
        }
        if let Some((name, value)) = scope.current_iter_binding() {
            let map = snapshot.get_or_insert_with(HashMap::new);
            // Outer (later-visited) bindings must NOT overwrite an
            // inner binding of the same name — Rust-like shadowing.
            map.entry(name).or_insert(value);
        }
        current = scope.parent.clone();
    }
    match snapshot {
        Some(locals) if !locals.is_empty() => base.with_locals(locals),
        _ => Arc::clone(base),
    }
}

fn fallback_parse_analyze(
    source: &ModuleSource,
    range: TokenRange,
) -> Result<Arc<Node>, RuntimeError> {
    let node = parse_document(&source.source).map_err(|error| RuntimeError::ModuleParseError {
        path: source.canonical_id.clone(),
        message: error.to_string(),
        range: range.into(),
    })?;
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
    Ok(Arc::new(node))
}

/// Verify a `#import "..." sha256:"..."` integrity pin against the
/// resolved module body bytes. review-improvement-174 (v3++ b-2 fix):
/// closes the analyzer-bypass attack vector where a host parses and
/// hands an unverified module directly to the evaluator.
///
/// Fail-closed semantics, in source order:
/// * Unknown algorithm → `ImportHashUnknownAlgorithm` (refuse before
///   we compute anything we cannot compare against).
/// * Malformed hex (wrong length or non-hex chars) → `ImportHashInvalidHex`.
/// * Digest mismatch → `ImportHashMismatch`.
/// * Match → `Ok(())`.
///
/// The check intentionally runs on every path (local, `std/...`, or
/// remote). Local pins are rare but legitimate (lockfile-style audit
/// of a vendored module); making the check path-agnostic means the
/// attacker cannot find a path shape that silently skips verification.
fn verify_module_integrity(
    raw_path: &str,
    pin: &relon_parser::IntegrityHash,
    body: &[u8],
    range: TokenRange,
) -> Result<(), RuntimeError> {
    let Some(algo) = pin.algorithm else {
        return Err(RuntimeError::ImportHashUnknownAlgorithm {
            path: raw_path.to_string(),
            algorithm: pin.algorithm_text.clone(),
            range,
        });
    };
    let algo_str = algo.as_str();
    let expected_len = algo.hex_len();
    if pin.hex.len() != expected_len || !pin.hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(RuntimeError::ImportHashInvalidHex {
            path: raw_path.to_string(),
            algorithm: algo_str.to_string(),
            expected_len,
            got_len: pin.hex.len(),
            range,
        });
    }
    let computed = compute_module_digest(algo, body);
    // Case-insensitive comparison mirrors the analyzer-side
    // `digest_matches` so a copy-pasted uppercase digest still
    // verifies. Pins themselves are usually lower-case hex.
    if !pin.hex.eq_ignore_ascii_case(&computed) {
        return Err(RuntimeError::ImportHashMismatch {
            payload: Box::new(relon_eval_api::error::ImportHashMismatchDetail {
                path: raw_path.to_string(),
                algorithm: algo_str.to_string(),
                expected: pin.hex.to_ascii_lowercase(),
                got: computed,
            }),
            range,
        });
    }
    Ok(())
}

/// Compute the lower-case hex digest of `bytes` under `algo`. Mirrors
/// `relon-analyzer::workspace_build::compute_digest` so the two
/// verification sites cannot drift on byte ordering or casing.
fn compute_module_digest(algo: relon_parser::HashAlgorithm, bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    match algo {
        relon_parser::HashAlgorithm::Sha256 => {
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            hex::encode(hasher.finalize())
        }
    }
}

/// Caps handle handed to native functions so they can call back into Relon.
///
/// Holds an `Arc<Context>` so the trait object is `'static` and the call-back
/// path can run a fresh [`TreeWalkEvaluator`] over the same shared context.
/// Cheap to keep around — every clone is just an Arc bump.
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
        let evaluator = TreeWalkEvaluator::new(Arc::clone(&self.context));
        let evaluated_args = args.into_iter().map(EvaluatedArg::positional).collect();
        let scope = Arc::clone(evaluator.empty_scope());
        evaluator.call_function_by_value(func.clone(), evaluated_args, &scope, range)
    }

    fn max_value_elements(&self) -> Option<usize> {
        self.context.capabilities.max_value_elements
    }

    fn next_iter_id(&self) -> u64 {
        self.context.next_iter_id()
    }

    fn iter_cursor_fetch_and_inc(&self, iter_id: u64, len: usize) -> Option<usize> {
        self.context.iter_cursor_fetch_and_inc(iter_id, len)
    }

    /// Charge `n` against the same `step_counter` the AST-node loop in
    /// `eval_internal` increments. Single source of truth — no parallel
    /// counter, no separate cap. Hot-path shape: `fetch_add` first, post-
    /// check second, mirroring `eval_internal` near `eval.rs:870`.
    fn tick(&self, n: u64, range: TokenRange) -> Result<(), RuntimeError> {
        let Some(limit) = self.context.capabilities.max_steps else {
            return Ok(());
        };
        let prev = self.context.step_counter.fetch_add(n, Ordering::Relaxed);
        // `prev` is the count *before* this tick. Trip the gate as soon
        // as the post-tick value exceeds the limit — matches the per-
        // AST-node check's "prev >= limit ⇒ fail" shape (both bail when
        // the budget is fully consumed, with a one-step grace at the
        // boundary inherited from the existing check).
        if prev.saturating_add(n) > limit {
            return Err(RuntimeError::StepLimitExceeded { limit, range });
        }
        Ok(())
    }
}

pub(crate) fn decorator_name(dec: &DecoratorNode) -> String {
    dec.path
        .iter()
        .map(|k| k.to_string_key())
        .collect::<Vec<_>>()
        .join(".")
}

/// True if `t` is a bare reference to one of `generics` — a
/// single-segment path with no nested generics whose name appears in
/// the generic-parameter list. Used by `eval_variant_ctor` to skip
/// validation of fields whose type is still an unresolved type
/// variable (the substitution arrives at the use site).
fn is_type_variable(t: &relon_parser::TypeNode, generics: &[String]) -> bool {
    t.path.len() == 1 && t.generics.is_empty() && generics.iter().any(|g| g == &t.path[0])
}

/// Schema-rooted Phase B: extract the schema name a value should be
/// dispatched against when used as a method-call receiver.
///
///   * `Value::Dict { brand: Some(name) }` — branded after schema
///     validation, dispatch on the brand.
///   * `Value::Dict { brand: None }` — unbranded dict falls back to
///     the built-in `"Dict"` tag so stdlib methods registered via
///     `register_pure_method("Dict", "keys", ...)` (Phase D 收尾) can
///     dispatch without requiring a user-side schema brand. Mirrors
///     the `String` / `List` arms below — they're equally schemaless
///     primitives and dispatch on the type name.
///   * Primitive values — dispatch on the built-in tag (`String`,
///     `Int`, …); aligns with `#extend String with { ... }`.
///   * `Value::Closure` / `Value::Schema` / `Value::Type` — no
///     receiver dispatch; `None` so callers know to skip.
fn value_schema_tag(v: &Value) -> Option<String> {
    match v {
        Value::Dict(d) => Some(d.brand.clone().unwrap_or_else(|| "Dict".to_string())),
        Value::Bool(_) => Some("Bool".to_string()),
        Value::Int(_) => Some("Int".to_string()),
        Value::Float(_) => Some("Float".to_string()),
        Value::String(_) => Some("String".to_string()),
        Value::List(_) => Some("List".to_string()),
        Value::Null => Some("Null".to_string()),
        Value::Closure(_) | Value::Schema(_) | Value::EnumSchema(_) => None,
        Value::Type(_) | Value::Wildcard => None,
    }
}

/// Phase C (Indexable lowering, decision 22): unwrap the `Optional<V>`
/// returned by a witness `index(key) -> Optional<V>` body into the
/// shape the surrounding `a[i]` / `a[i]?` site expects.
///
/// The shape is `variant_dict(map, "Some" | "None", "Option")` — built
/// by the prelude's `Option<T>` enum schema and the stdlib's
/// [`option_value`] constructor. Wrapped success returns the inner
/// `value`; a `None` result either becomes `Value::Null` (when the
/// caller used `a[i]?`) or surfaces as `VariableNotFound` (matching the
/// existing dict / list miss diagnostic for `a[i]` without `?`).
///
/// Non-Option-shaped returns pass through verbatim: the analyzer's
/// constraint-witness shape check (`constraints.rs::Indexable` →
/// `return_type: "Optional"`) already gates source-level
/// `#derive Indexable` to `index() -> Optional<...>`, so reaching this
/// helper with a non-Option value implies a host-registered native
/// method that bypassed the source-side check — surfacing it as-is is
/// the only safe move (we don't have the original type to coerce
/// against).
pub(crate) fn unwrap_optional_for_index(
    raw: Value,
    is_optional: bool,
    display_name: &str,
    range: TokenRange,
) -> Result<Value, RuntimeError> {
    if let Value::Dict(d) = &raw {
        if d.variant_of.as_deref() == Some("Option") {
            match d.brand.as_deref() {
                Some("Some") => {
                    return Ok(d.map.get("value").cloned().unwrap_or(Value::Null));
                }
                Some("None") => {
                    return if is_optional {
                        Ok(Value::Null)
                    } else {
                        Err(RuntimeError::VariableNotFound(
                            display_name.to_string(),
                            range,
                        ))
                    };
                }
                _ => {}
            }
        }
    }
    // Non-Option return: pass through. See doc-comment rationale.
    Ok(raw)
}

/// True when `node` carries the `#private` directive. See
/// [`crate::decorator_names::PRIVATE`] for the field-level semantics.
pub(crate) fn is_private_field(node: &Node) -> bool {
    node.directives
        .iter()
        .any(|dir| dir.name == crate::decorator_names::PRIVATE)
}

// `impl Display for Value` moved to `relon_eval_api::value` so that it
// lives in the same crate as the `Value` type (Rust orphan rule).

/// Install the tree-walk backend's default stdlib / decorators / prelude
/// into a context the first time it is wrapped by a [`TreeWalkEvaluator`].
///
/// The check is gated by [`Context::backend_prepared`]: the first wrap
/// flips it from `false` to `true`, so subsequent wraps (or a host that
/// reuses the same `Arc<Context>` across multiple evaluator instances)
/// are no-ops. Defaults are inserted with the "user's prior registration
/// wins" semantics — `register_*` helpers on `Context` use
/// `HashMap::insert`, so the runtime ordering is: user calls happen
/// before this wrap, this wrap installs defaults *after*, but only into
/// slots the user hasn't filled (the install helpers below short-circuit
/// when the key is already present).
///
/// `sandboxed_flag` triggers the default-deny `FilesystemModuleResolver`
/// after the virtual `std/...` resolver, mirroring the pre-split
/// `Context::sandboxed` behavior.
///
/// Panics if the input `Arc` is shared (strong_count > 1) and
/// `backend_prepared` is still false: there is no way to mutate a shared
/// `Arc` to install the defaults, so silently skipping would surface
/// later as `RuntimeError::FunctionNotFound` for stdlib methods. The
/// fix is for the host to call [`TreeWalkEvaluator::prepare_in_place`]
/// on the bare `Context` before the first `Arc::new(ctx)`.
fn prepare_tree_walk_context(mut context: Arc<Context>) -> Arc<Context> {
    if context.backend_prepared {
        return context;
    }
    let ctx = Arc::get_mut(&mut context).unwrap_or_else(|| {
        panic!(
            "TreeWalkEvaluator::new received an Arc<Context> that is shared \
             (strong_count > 1) but the tree-walking backend has not been \
             prepared yet. Either: \
             (a) build the Arc immediately before constructing the evaluator \
             (so it is uniquely owned when wrapped), or \
             (b) call TreeWalkEvaluator::prepare_in_place(&mut ctx) on the \
             bare Context before the first Arc::new(ctx)."
        )
    });
    TreeWalkEvaluator::prepare_in_place(ctx);
    context
}

// Forward the trait surface defined in `relon-eval-api` onto the
// concrete tree-walking implementation. Hosts that want to swap
// backends store `Box<dyn relon_eval_api::Evaluator>`; hosts that want
// to call backend-specific methods keep the concrete type.
impl relon_eval_api::Evaluator for TreeWalkEvaluator {
    fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        TreeWalkEvaluator::eval(self, node, scope)
    }

    fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        TreeWalkEvaluator::eval_root(self, scope)
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        let scope = Arc::clone(self.empty_scope());
        TreeWalkEvaluator::run_main(self, &scope, args)
    }

    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        TreeWalkEvaluator::force_thunk(self, thunk)
    }

    fn invoke_closure(
        &self,
        closure: &relon_eval_api::ClosureData,
        args: &[Value],
    ) -> Result<Value, RuntimeError> {
        // Wrap positional values into the `EvaluatedArg` shape `eval_closure`
        // expects; closures called through the trait surface never carry
        // named args (`#main`-style binding stays on the explicit
        // `run_main` entry point).
        let evaluated_args = args.iter().cloned().map(EvaluatedArg::positional).collect();
        self.eval_closure(
            &closure.params,
            &closure.body,
            evaluated_args,
            &closure.captured_env,
            closure.body.range,
        )
    }
}
