//! Type-check sub-module: function-call / method-dispatch / index-dispatch
//! checks.
//!
//! All eight methods extend [`super::Walker`] in place — they touch
//! the same mutable state (`tree.diagnostics`, `scope_stack`,
//! `schema_index`, `base_index`, `pipe_target_calls`) that the
//! dispatch loop in `mod.rs` carries, so we use a sibling `impl<'a>
//! super::Walker<'a>` block rather than abstracting through a trait.
//!
//! Method grouping:
//! * `check_unresolved_fn_call` — Stage 2.7 bare-name FnCall whose
//!   head can't be resolved against any scope.
//! * `check_fn_call` — Stage 3.5/3.6 signature-aware arity + per-arg
//!   type validation (with v1.1 generic unification).
//! * `resolve_call_signature` — central lookup that fans out across
//!   single-name → sibling-dict closure → cross-module alias →
//!   schema-method tables.
//! * `check_method_dispatch` — Schema-rooted Phase B `UnknownMethod`
//!   + `PrivateMethodViolation` emitter.
//! * `check_index_dispatch` — Schema-rooted §J Dynamic-segment
//!   `key` param subsumption check.
//! * `in_method_block`, `resolve_method_receiver`,
//!   `resolve_method_receiver_prefix` — small receiver-resolution
//!   helpers shared by the method/index dispatch methods.

use super::helpers::{format_type, param_is_polymorphic, required_and_max};
use super::Walker;
use crate::diagnostic::{span_of, Diagnostic};
use crate::generics::collect_bindings;
use crate::infer::{self, infer_type, InferredType};
use crate::sig::{instantiate, lookup_signature, FnSignature};
use relon_parser::{CallArg, Expr, Node, TokenKey, TokenRange};
use std::collections::HashMap;

impl<'a> Walker<'a> {
    /// Stage 2.7: when a function call's `callable` is a single-segment
    /// bare name and the analyzer can prove the name isn't bound — not
    /// a closure param, not a sibling, not in `host_fn_names ∪
    /// stdlib_names()` — surface `UnresolvedReference`. Multi-segment
    /// callables (`obj.method(...)`) are deferred to a later stage.
    pub(super) fn check_unresolved_fn_call(&mut self, node: &Node, path: &[TokenKey]) {
        if path.len() != 1 {
            return;
        }
        let TokenKey::String(name, _, _) = &path[0] else {
            return;
        };
        if self.dynamic_save(name) || self.is_known_fn(name) {
            return;
        }
        // Sibling-bound name? `{ helper(): 1, x: helper() }` — the
        // resolver pass binds `helper` as a Variable head, but `FnCall`
        // doesn't go through `Reference`/`Variable`, so we have to walk
        // the scope chain ourselves.
        for frame in self.scope_stack.iter().rev() {
            if frame.fields.contains_key(name) || frame.closure_params.contains_key(name) {
                return;
            }
        }
        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
            name: name.clone(),
            range: span_of(node.range),
        });
    }

    /// Stage 3.5 / 3.6: validate a `FnCall` against its static
    /// signature when one is reachable. Three sources, in order:
    ///
    /// 1. A multi-segment path whose head resolves to a sibling dict
    ///    field whose value is a closure (Stage 3.6 dict-literal
    ///    sibling form).
    /// 2. A single-segment name resolved through
    ///    [`lookup_signature`] (closure index → host fns → stdlib).
    ///
    /// Anything not in those tables silently passes — runtime keeps
    /// owning the verdict for cross-module fns, dynamic refs, etc.
    pub(super) fn check_fn_call(
        &mut self,
        node: &Node,
        path: &[TokenKey],
        args: &[relon_parser::CallArg],
    ) {
        // Pipe RHS: the LHS supplies the implicit first arg, so the
        // source-level arity is intentionally one short. Skipping
        // here keeps the analyzer in lock-step with the runtime's
        // pipe semantics (`call_function` prepends the LHS value).
        if self.pipe_target_calls.contains(&node.id) {
            return;
        }
        let Some(sig) = self.resolve_call_signature(path) else {
            return;
        };
        let display_name = sig.name.clone();
        let positional_count = args.iter().filter(|a| a.name.is_none()).count();
        // v1 only validates positional args; named args silently pass
        // (they would just shadow positions or be redundant). Bail out
        // early when any named arg is present so we don't false-flag.
        if positional_count != args.len() {
            return;
        }
        // Arity check honoring optional tail params and variadic_tail.
        let (required, max_fixed) = required_and_max(&sig);
        let in_range = if sig.variadic_tail.is_some() {
            args.len() >= required
        } else {
            args.len() >= required && args.len() <= max_fixed
        };
        if !in_range {
            let expected = if sig.variadic_tail.is_some() {
                format!("at least {required}")
            } else if required == max_fixed {
                format!("{required}")
            } else {
                format!("{required} to {max_fixed}")
            };
            self.tree
                .diagnostics
                .push(Diagnostic::FnCallArgCountMismatch {
                    fn_name: display_name,
                    expected,
                    found: args.len(),
                    range: span_of(node.range),
                });
            return;
        }

        // Per-arg type check. Arguments past the fixed list are
        // validated against `variadic_tail`. Arguments mapped to
        // optional params still check, but a missing optional is fine
        // (already accepted above by `required <= len`).
        //
        // We collect diagnostics into a local Vec so the inference
        // scope's read-only borrow of `self.tree` doesn't collide with
        // the diagnostic push's mutable borrow.
        //
        // v1.1: when `sig.generics` is non-empty, run unification over
        // every (param_ty, arg_ty) pair first to collect placeholder
        // bindings, then instantiate the signature so each per-arg
        // subsumption check sees a concrete type. The substitution
        // is shared with the FnCall return-type inference path in
        // `infer::infer_type` so the rest of the analyzer reads the
        // tightened type back out as `List<Int>` etc.
        let mut to_emit: Vec<Diagnostic> = Vec::new();
        {
            let scope = self.build_type_scope();
            let working_sig = if sig.generics.is_empty() {
                sig.clone()
            } else {
                let bindings = collect_bindings(&sig, args, &scope);
                instantiate(&sig, &bindings)
            };
            for (idx, arg) in args.iter().enumerate() {
                let (param_name, expected_ty) = if idx < working_sig.params.len() {
                    (
                        working_sig.params[idx].name.clone(),
                        working_sig.params[idx].ty.clone(),
                    )
                } else if let Some(tail_ty) = &working_sig.variadic_tail {
                    (
                        format!("rest[{}]", idx - working_sig.params.len()),
                        tail_ty.clone(),
                    )
                } else {
                    continue;
                };
                let Some(arg_ty) = infer_type(&arg.value, &scope) else {
                    continue;
                };
                if arg_ty.subsumes_with_imports(
                    &expected_ty,
                    Some(&self.base_index),
                    self.tree.workspace_import_index.as_ref(),
                ) {
                    continue;
                }
                to_emit.push(Diagnostic::FnCallArgTypeMismatch {
                    fn_name: display_name.clone(),
                    param_name,
                    expected: format_type(&expected_ty),
                    found: arg_ty.name(),
                    range: span_of(arg.value.range),
                });
            }
        }
        self.tree.diagnostics.extend(to_emit);
    }

    /// Stage 3.5: resolve a call path to its static signature.
    /// Single-segment paths go through the global lookup
    /// (closure-index → host fns → stdlib). Multi-segment paths fall
    /// back to the Stage 3.6 dict-literal sibling form: the head must
    /// name a sibling whose value is a Dict literal, and the second
    /// segment must name a key in that dict whose value is a closure.
    /// Anything else returns `None` (silent fall-through).
    pub(super) fn resolve_call_signature(&self, path: &[TokenKey]) -> Option<FnSignature> {
        if path.is_empty() {
            return None;
        }
        // Head must be a String key — Spread / synthetic keys can never
        // produce a callable.
        let TokenKey::String(head, _, _) = &path[0] else {
            return None;
        };
        if path.len() == 1 {
            return lookup_signature(head, self.tree, &self.tree.host_fn_signatures);
        }
        // 2-segment paths: dict-literal sibling closure, cross-module
        // aliased closure, and the head-as-schema dispatch all live
        // here. 3+ segment paths fall through to schema-rooted
        // multi-hop dispatch at the bottom (no sibling-dict /
        // aliased-import shape applies there — those tables are keyed
        // by head name alone).
        let last_idx = path.len() - 1;
        let TokenKey::String(method, _, _) = &path[last_idx] else {
            return None;
        };
        if path.len() == 2 {
            // Try sibling-field dict-literal first (Stage 3.6). Falls
            // through to the v1.1 cross-module index when there's no
            // sibling field with that name.
            if let Some(target) = self.lookup_field_node(head) {
                // Only walk literal dicts — abstract types (FnCall /
                // Reference / typed schema bindings) silently fall
                // through to runtime.
                if target.type_hint.is_none() {
                    if let Expr::Dict(inner_pairs) = &*target.expr {
                        for (k, v) in inner_pairs {
                            if let TokenKey::String(name, _, _) = k {
                                if name == method {
                                    if matches!(&*v.expr, Expr::Closure { .. }) {
                                        let mut sig =
                                            self.tree.closure_signatures.get(&v.id).cloned()?;
                                        sig.name = format!("{head}.{method}");
                                        return Some(sig);
                                    }
                                    return None;
                                }
                            }
                        }
                    }
                }
            }
            // v1.1: cross-module alias.method — `#import alias from
            // "lib"` exposes the imported module's top-level closures
            // under `alias.method`.
            if let Some(idx) = self.tree.workspace_import_index.as_ref() {
                if let Some(methods) = idx.aliased_closures.get(head) {
                    if let Some(sig) = methods.get(method) {
                        return Some(sig.clone());
                    }
                }
            }
        }
        // Schema-rooted Phase B: schema method dispatch. Three flavors:
        //
        //   1. `value.method(args)` — `value` is a 1-segment binding
        //      whose static type is some `Schema(name)`. Look up the
        //      method on that schema's table.
        //   2. `Schema.method(args)` — `Schema` itself is the head,
        //      static (no receiver) dispatch; lookup keyed by the
        //      schema name regardless.
        //   3. `head.f1.f2…fk.method(args)` (multi-hop) — walk
        //      `path[..-1]` through the inference engine; when it
        //      terminates on `Schema(name)` we dispatch the last
        //      segment against that schema's method table. Mirrors
        //      the v1.4 path-tail machinery `infer::walk_path`
        //      already powers for spreads / strict mode.
        //
        // For case 1, the receiver type comes from the path-walker so
        // schema-typed sibling fields, schema-typed `#main(p: T)`
        // params, and `let`-style closure params all participate.
        if let Some(receiver_schema) = self.resolve_method_receiver_prefix(&path[..last_idx]) {
            if let Some(sig) = self
                .tree
                .method_signatures
                .get(&(receiver_schema, method.clone()))
            {
                return Some(sig.clone());
            }
        }
        None
    }

    /// Schema-rooted Phase B: emit `UnknownMethod` when a 2-segment
    /// `head.method(...)` call has a head whose static type resolves
    /// to a known schema, but the method isn't recorded on that
    /// schema's table. Also enforces `#internal` visibility — an internal
    /// method may only be called from another method on the *same*
    /// schema (currently approximated as: from inside the same
    /// `with { ... }` block, tracked via `self.method_call_context`).
    ///
    /// Skipped when the head doesn't resolve to a schema receiver: the
    /// existing single-segment `UnresolvedReference` /
    /// `UnknownTypeName` machinery already covers that, and we must
    /// not double-count names like sibling closures or aliased imports.
    pub(super) fn check_method_dispatch(&mut self, _node: &Node, path: &[TokenKey]) {
        if path.len() < 2 {
            return;
        }
        let TokenKey::String(head, _, _) = &path[0] else {
            return;
        };
        let last_idx = path.len() - 1;
        let TokenKey::String(method, method_range, _) = &path[last_idx] else {
            return;
        };
        // For the 2-segment form only, head-as-sibling-closure or
        // head-as-aliased-import is a non-schema dispatch — let the
        // regular signature checks handle it. With 3+ segments, the
        // head is necessarily the root of a path walk (sibling
        // closures and aliased imports don't have nested fields the
        // walker would descend through), so we keep the multi-hop
        // path-walk authoritative.
        if path.len() == 2 {
            if self.lookup_field_node(head).is_some() {
                return;
            }
            if let Some(idx) = self.tree.workspace_import_index.as_ref() {
                if idx.aliased_closures.contains_key(head) {
                    return;
                }
            }
        }
        let Some(schema) = self.resolve_method_receiver_prefix(&path[..last_idx]) else {
            return;
        };
        let Some(info) = self
            .tree
            .schema_methods
            .get(&schema)
            .and_then(|methods| methods.iter().find(|m| &m.name == method))
            .cloned()
        else {
            self.tree.diagnostics.push(Diagnostic::UnknownMethod {
                schema,
                method: method.clone(),
                range: span_of(*method_range),
            });
            return;
        };
        if info.is_private && !self.in_method_block(&schema) {
            self.tree
                .diagnostics
                .push(Diagnostic::PrivateMethodViolation {
                    schema,
                    method: method.clone(),
                    range: span_of(*method_range),
                });
        }
    }

    /// Schema-rooted §J follow-up: walk `path` for any `Dynamic`
    /// segments. Each one is the bracket-form `a[expr]` desugar
    /// landing site — at runtime it dispatches through the receiver
    /// schema's `index(key: ...)` witness. When that witness's
    /// declared key parameter has a concrete type (after constraint-
    /// generic substitution, e.g. `index(key: Int)`), validate the
    /// dynamic key's inferred type against it. Mismatch surfaces as
    /// `MethodGenericArgMismatch`; without this check the same
    /// disagreement only fires at runtime from inside the witness
    /// body.
    ///
    /// The receiver type is recovered by walking `path[..i]` through
    /// `walk_path`. If the prefix doesn't land on a schema we have a
    /// method table for (or the schema has no `index` method, or its
    /// `index` declares `key: K` with `K` still polymorphic), we
    /// silently skip — runtime still owns those cases.
    pub(super) fn check_index_dispatch(&mut self, path: &[TokenKey]) {
        // Walk the segments looking for a Dynamic — each one is an
        // independent index call against the prefix up to (but not
        // including) the segment itself.
        let scope = self.build_type_scope();
        let mut to_emit: Vec<Diagnostic> = Vec::new();
        for (idx, seg) in path.iter().enumerate() {
            let TokenKey::Dynamic(key_node, _is_optional) = seg else {
                continue;
            };
            if idx == 0 {
                // A leading Dynamic would mean `[expr]` at the root,
                // which the parser never produces — but be defensive.
                continue;
            }
            let prefix = &path[..idx];
            let schema_name = match infer::walk_path(prefix, &scope) {
                infer::PathTailOutcome::Resolved(InferredType::Schema(name)) => name,
                _ => continue,
            };
            // Locate the synthesized signature so we have both the
            // method's generics list and the param's TypeNode. The
            // method-signature table is the single source of truth
            // after `build_method_signature_table`.
            let key = (schema_name.clone(), "index".to_string());
            let Some(sig) = self.tree.method_signatures.get(&key).cloned() else {
                continue;
            };
            let Some(key_param) = sig.params.first() else {
                continue;
            };
            // Skip when the param type is still a polymorphic
            // placeholder. The receiver-schema's generics shadow the
            // method's own — both must be subtracted before declaring
            // a leftover name "polymorphic". Concrete types
            // (`Int`/`String`/builtins/user schemas) pass.
            if param_is_polymorphic(&key_param.ty, &sig.generics, &schema_name, self.tree) {
                continue;
            }
            let Some(arg_ty) = infer_type(key_node, &scope) else {
                continue;
            };
            // `Any` here is the closure-param-without-type leak (the
            // only Any source left after v1.6); don't double-flag.
            if matches!(arg_ty, InferredType::Any) {
                continue;
            }
            if arg_ty.subsumes_with_imports(
                &key_param.ty,
                Some(&self.base_index),
                self.tree.workspace_import_index.as_ref(),
            ) {
                continue;
            }
            to_emit.push(Diagnostic::MethodGenericArgMismatch {
                schema: schema_name,
                method: "index".to_string(),
                param_name: key_param.name.clone(),
                expected: format_type(&key_param.ty),
                found: arg_ty.name(),
                range: span_of(key_node.range),
            });
        }
        self.tree.diagnostics.extend(to_emit);
    }

    /// True when the walker is currently inside the body of a method
    /// declared on `schema`. The typecheck walker only traverses the
    /// entry root today — method bodies are not walked from this
    /// path — so the answer is always `false`. The `#internal` check
    /// in `check_method_dispatch` therefore only flags calls reached
    /// from the entry root, which is the correct conservative
    /// behaviour: private methods are explicitly opt-out of external
    /// callers. When a future pass walks method bodies, it must push
    /// a "currently inside schema X's method body" frame onto the
    /// walker so this hook returns `true` for sibling-method calls.
    pub(super) fn in_method_block(&self, _schema: &str) -> bool {
        false
    }

    /// Resolve the receiver schema name of a method call's head segment.
    /// Returns the schema name if either:
    ///   * `head` is itself a schema name in scope (the static
    ///     `Schema.method` form), or
    ///   * `head` is a binding whose static type chain ends at a
    ///     `Schema(name)`.
    pub(super) fn resolve_method_receiver(&self, head: &str) -> Option<String> {
        // Static `Schema.method(...)` form: head is a known schema
        // name. Any name registered in `schema_index` (whether via a
        // `#schema` directive, an enum, or a base reference) qualifies.
        if self.schema_index.contains_key(head) {
            return Some(head.to_string());
        }
        // Even if the schema has no `+ Base` chain (and hence no entry
        // in `schema_index`), a name explicitly recorded in
        // `schema_methods` is still a valid receiver root — that
        // covers schemas declared via `#schema X with { ... }` whose
        // body is empty / `Enum<...>` and never propagated into the
        // base index.
        if self.tree.schema_methods.contains_key(head) {
            return Some(head.to_string());
        }
        let scope = self.build_type_scope();
        let single_path = [TokenKey::String(
            head.to_string(),
            TokenRange::default(),
            false,
        )];
        if let infer::PathTailOutcome::Resolved(InferredType::Schema(name)) =
            infer::walk_path(&single_path, &scope)
        {
            return Some(name);
        }
        None
    }

    /// Multi-hop receiver resolution: walk `path[..-1]` and return the
    /// schema name when the walk lands on `Schema(name)`. The last
    /// segment is the method name and is *not* included in the walk.
    ///
    /// For `path.len() == 2` this reduces to single-name head lookup
    /// (mirrors [`Self::resolve_method_receiver`]).
    /// For `path.len() >= 3` (`o.customer.greet()`) we let `walk_path`
    /// descend through every intermediate field — the prefix that gets
    /// fed to `walk_path` is `[o, customer]`, and a successful
    /// `Resolved(Schema("User"))` makes `greet` dispatch against
    /// `User`'s method table.
    ///
    /// Returns `None` when the prefix walk doesn't terminate on a
    /// schema (`UnknownHead` / `UnknownStep` / non-schema final type).
    /// Callers fall through to whatever non-schema dispatch path they
    /// own — no new diagnostic is emitted here.
    pub(super) fn resolve_method_receiver_prefix(&self, prefix: &[TokenKey]) -> Option<String> {
        debug_assert!(!prefix.is_empty());
        if prefix.len() == 1 {
            let TokenKey::String(head, _, _) = &prefix[0] else {
                return None;
            };
            return self.resolve_method_receiver(head);
        }
        let scope = self.build_type_scope();
        if let infer::PathTailOutcome::Resolved(InferredType::Schema(name)) =
            infer::walk_path(prefix, &scope)
        {
            return Some(name);
        }
        None
    }

    /// R1 (contextual closure typing): inspect a `FnCall` site and, for
    /// every closure-literal argument whose param slot in the resolved
    /// signature is a `Closure<…>`, derive the closure's parameter types
    /// from the call context and stash them on
    /// `self.closure_param_context` keyed by the closure node's id. The
    /// `Expr::Closure` arm of `visit_internal` then reads them back so a
    /// param whose type IS derivable no longer trips
    /// `ClosureParamTypeMissing` under strict mode.
    ///
    /// Handles both call shapes that route to the same higher-order
    /// signatures:
    ///
    /// * **Bare form** — `_list_map(xs, (x) => …)`: resolved straight
    ///   through [`Self::resolve_call_signature`], args used as-is.
    /// * **Method form** — `xs.map((x) => …)` / `range(n).map(…)`: the
    ///   parser lowers this to a `FnCall` whose head is a
    ///   `TokenKey::Dynamic(receiver)` segment followed by the method
    ///   name. We map the list method name onto its `_list_*` intrinsic
    ///   signature and synthesize an arg list with the receiver
    ///   prepended as positional arg 0, so the exact same generic
    ///   unification that the bare form uses applies.
    pub(super) fn populate_closure_context(&mut self, path: &[TokenKey], args: &[CallArg]) {
        // Resolve the signature + the effective positional arg list for
        // unification. Two shapes feed the same machinery.
        let Some((sig, eff_args)) = self.contextual_call_resolution(path, args) else {
            return;
        };
        if sig.generics.is_empty() {
            // A monomorphic signature still pins closure params directly
            // from its declared `Closure<…>` slot (no unification
            // needed), but the stdlib higher-order fns are all generic,
            // so this is rare. We still handle it uniformly below by
            // running `collect_bindings` (returns empty for no generics)
            // and substituting (a no-op), so don't early-return.
        }

        // Run pass-1 generic unification off the non-closure args — this
        // is exactly what `collect_bindings` does, and we reuse it so the
        // `Closure<T, U>` slot's `T` resolves to the element type bound
        // from the `List<T>` receiver / init arg.
        let bindings = {
            let scope = self.build_type_scope();
            collect_bindings(&sig, &eff_args, &scope)
        };

        // For each closure-literal arg whose slot is `Closure<…>`,
        // substitute the slot's leading (param) type vars under the
        // bindings and record the concrete ones.
        let mut to_record: Vec<(
            relon_parser::NodeId,
            HashMap<String, relon_parser::TypeNode>,
        )> = Vec::new();
        for (idx, arg) in eff_args.iter().enumerate() {
            let Expr::Closure { params, .. } = &*arg.value.expr else {
                continue;
            };
            let Some(param_ty) = (if idx < sig.params.len() {
                Some(&sig.params[idx].ty)
            } else {
                sig.variadic_tail.as_ref()
            }) else {
                continue;
            };
            if param_ty.path.len() != 1 {
                continue;
            }
            let head = param_ty.path[0].as_str();
            if head != "Closure" && head != "Fn" {
                continue;
            }
            if param_ty.generics.is_empty() {
                continue;
            }
            // Slot layout: [param_tys.., return_ty]. Only the leading
            // param slots feed closure-param typing.
            let (slot_param_tys, _ret) = param_ty.generics.split_at(param_ty.generics.len() - 1);
            let mut derived: HashMap<String, relon_parser::TypeNode> = HashMap::new();
            for (p_idx, cp) in params.iter().enumerate() {
                // An explicit annotation already owns this param — no
                // contextual derivation needed (and the strict guard
                // doesn't fire on it anyway).
                if cp.type_hint.is_some() {
                    continue;
                }
                let Some(slot) = slot_param_tys.get(p_idx) else {
                    continue;
                };
                let mut sub = slot.clone();
                crate::sig::substitute_in_type_node(&mut sub, &sig.generics, &bindings);
                // Only pin params whose type resolved to something
                // concrete. A residual `Any` (unbindable generic) is left
                // unpinned so the strict guard still rejects the true
                // leak.
                if matches!(
                    crate::infer::infer_from_type_node(&sub),
                    crate::infer::InferredType::Any
                ) {
                    continue;
                }
                derived.insert(cp.name.clone(), sub);
            }
            if !derived.is_empty() {
                to_record.push((arg.value.id, derived));
            }
        }
        for (id, derived) in to_record {
            self.closure_param_context.insert(id, derived);
        }
    }

    /// Resolve a call site to `(signature, effective positional args)`
    /// for R1 contextual closure typing. Covers the bare form and the
    /// `TokenKey::Dynamic`-head list-method form. Returns `None` when no
    /// higher-order signature applies (so contextual typing is a no-op
    /// and the strict guard keeps owning the verdict).
    fn contextual_call_resolution(
        &self,
        path: &[TokenKey],
        args: &[CallArg],
    ) -> Option<(FnSignature, Vec<CallArg>)> {
        // Bare form: every segment is a String — `resolve_call_signature`
        // already fans out across closure-index / host / stdlib / schema
        // method tables.
        if path.iter().all(|s| matches!(s, TokenKey::String(_, _, _))) {
            let sig = self.resolve_call_signature(path)?;
            return Some((sig, args.to_vec()));
        }
        // Method form: `<receiver>.method(args)` lowers to
        // `[Dynamic(receiver), String(method)]`. Map the recognized list
        // methods onto their `_list_*` intrinsic and prepend the receiver
        // as positional arg 0.
        if path.len() != 2 {
            return None;
        }
        let TokenKey::Dynamic(receiver, _) = &path[0] else {
            return None;
        };
        let TokenKey::String(method, _, _) = &path[1] else {
            return None;
        };
        let intrinsic = match method.as_str() {
            "map" => "_list_map",
            "filter" => "_list_filter",
            "reduce" => "_list_reduce",
            "contains" => "_list_contains",
            _ => return None,
        };
        let sig = lookup_signature(intrinsic, self.tree, &self.tree.host_fn_signatures)?;
        let mut eff_args: Vec<CallArg> = Vec::with_capacity(args.len() + 1);
        eff_args.push(CallArg {
            name: None,
            value: receiver.clone(),
        });
        eff_args.extend(args.iter().cloned());
        Some((sig, eff_args))
    }
}
