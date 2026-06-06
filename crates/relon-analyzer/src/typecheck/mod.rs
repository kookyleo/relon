//! Static type-check pass.
//!
//! Statically classifiable type errors are surfaced as `Error`-severity
//! diagnostics — the analyzer is the source of truth for anything
//! derivable from source + schemas alone, and the facade refuses to
//! enter the evaluator while errors are present (see
//! `Diagnostic::severity`). The evaluator's runtime `check_type` only
//! handles the residual cases the static pass can't see (host-pushed
//! `#main(...)` args, dynamic `#brand`, `Match` arms over a runtime
//! value, etc.).
//!
//! Two complementary findings live here:
//!
//! * [`UnresolvedReference`] — a `&sibling.X` / `Variable(X)` whose
//!   head couldn't be statically bound *and* no spread / closure
//!   binding on the active scope chain could plausibly save it.
//!   Surfaced as `Warning` because the analyzer's view is conservative.
//! * [`StaticTypeMismatch`] — a typed schema binding whose value
//!   expression has a determinable shape (literal, list, dict, type)
//!   that disagrees with the field's declared type. Surfaced as
//!   `Error`.
//!
//! [`UnresolvedReference`]: crate::Diagnostic::UnresolvedReference
//! [`StaticTypeMismatch`]: crate::Diagnostic::StaticTypeMismatch
//!
//! ## Walker pass map
//!
//! The `Walker` struct owns three pieces of mutable state shared by
//! every check method: `tree.diagnostics`, `scope_stack`, and
//! `schema_index` (plus the smaller `enum_index`,
//! `variant_field_index`, `base_index`, `pipe_target_calls`
//! side-tables). The dispatch loop (`visit_internal`) lives in this
//! file; every domain check is implemented on a sibling sub-module
//! via an `impl<'a> super::Walker<'a>` extension block, so each
//! method retains direct access to the same private fields without
//! a trait abstraction.
//!
//! Sub-module split (P3-phase2):
//!
//! - **`helpers`** — pure free fns (`format_type`, `levenshtein`,
//!   `closest_variant`, `same_outer_container`, `required_and_max`,
//!   `extract_closure_signature`, `param_is_polymorphic`,
//!   `stdlib_registered_names`, `stdlib_names`) + small Walker
//!   scaffolding (`build_type_scope` / `is_known_fn` /
//!   `lookup_field_node` / `dynamic_save`).
//! - **`index`** — pre-walk side-table builders
//!   (`build_schema_index` / `build_enum_index` /
//!   `build_variant_field_index` / `build_base_index` /
//!   `main_param_frame_for_typecheck` / `collect_pipe_target_calls`)
//!   plus the `SchemaIndex` / `EnumIndex` / `VariantFieldIndex`
//!   aliases and `substitute_generics_in_typenode`.
//! - **`fn_call`** — `check_unresolved_fn_call`, `check_fn_call`,
//!   `resolve_call_signature`, `check_method_dispatch`,
//!   `check_index_dispatch`, `in_method_block`,
//!   `resolve_method_receiver`, `resolve_method_receiver_prefix`.
//! - **`binary`** — `check_binary_mismatch`, `check_const_fold`,
//!   `check_strict_fn_call`.
//! - **`spread`** — `check_dict_v1_3`, `schema_known`,
//!   `spread_source_schema`, `spread_source_known_non_dict`,
//!   `spread_source_is_dict`, `spread_contributed_keys`.
//! - **`pattern`** — `check_match_arm_types`, `check_closure_return`,
//!   `check_match_exhaustiveness`, `infer_enum_type`.
//! - **`reference`** — `check_unresolved_ref`, `check_unresolved_var`,
//!   `check_strict_path`, `check_path_tail`.
//! - **`typed_binding`** — `check_typed_binding`, `check_generics`,
//!   `check_against_custom_schema`, `build_generic_subst`.
//!
//! `tests.rs` carries the integrated test suite (the assertions
//! exercise every group through the public `typecheck` entry, so the
//! domain extractions stay verifiable end-to-end).

mod binary;
mod fn_call;
mod helpers;
mod index;
mod pattern;
mod reference;
mod spread;
mod typed_binding;

#[cfg(test)]
mod tests;

pub use helpers::format_type;
pub use index::substitute_generics_in_typenode;
pub(crate) use index::{build_base_index, build_schema_index, SchemaIndex};

use helpers::extract_closure_signature;
use index::{
    build_enum_index, build_variant_field_index, collect_pipe_target_calls,
    main_param_frame_for_typecheck, EnumIndex, VariantFieldIndex,
};

use crate::diagnostic::{span_of, Diagnostic};
use crate::infer::{infer_type, InferredType, SchemaBaseIndex};
use crate::resolve::{build_frame, ScopeFrame};
use crate::tree::AnalyzedTree;
use relon_parser::{child_nodes, Expr, Node, TokenKey};
use std::collections::HashSet;

/// Run the type-check walker over `root` and append diagnostics to
/// `tree`. Must be called after [`crate::resolve::resolve_references`]
/// and [`crate::schema::collect_schemas`] so the side-tables they
/// produce are available.
pub fn typecheck(root: &Node, tree: &mut AnalyzedTree) {
    // Collect static type info and field bindings for each declared
    // schema so the value-type pass can look fields up by name.
    let schema_index = build_schema_index(tree);
    let enum_index = build_enum_index(tree);
    let variant_field_index = build_variant_field_index(tree);
    let base_index = build_base_index(tree);

    let mut pipe_target_calls = HashSet::new();
    collect_pipe_target_calls(root, &mut pipe_target_calls);

    // v1.3: seed the entry's `#main(...)` parameters into a synthetic
    // root frame so the type-check walker sees them as bindings with
    // their declared types — same logic the resolver uses (mirrors
    // `crate::resolve::main_param_frame`). Without this, root bodies
    // like `#main(Int n) -> String\nn+1` would have `n` typed as `Any`
    // and the `MainReturnTypeMismatch` check would silently skip.
    let mut scope_stack: Vec<ScopeFrame> = Vec::new();
    if let Some(frame) = main_param_frame_for_typecheck(tree, root.id) {
        scope_stack.push(frame);
    }

    let mut walker = Walker {
        tree,
        scope_stack,
        schema_index,
        enum_index,
        variant_field_index,
        base_index,
        pipe_target_calls,
        closure_param_context: std::collections::HashMap::new(),
    };
    walker.visit(root);
}

struct Walker<'a> {
    tree: &'a mut AnalyzedTree,
    scope_stack: Vec<ScopeFrame>,
    schema_index: SchemaIndex,
    enum_index: EnumIndex,
    /// v1.8 C3: per-variant field types for sum-type schemas, used by
    /// `check_generics` to recurse into a variant constructor literal
    /// (`Result.Ok { value: 42 }`) against a typed slot
    /// (`Result<Int, String>`) with proper generic substitution.
    variant_field_index: VariantFieldIndex,
    base_index: SchemaBaseIndex,
    /// Stage 3.5: NodeIds of FnCall expressions that appear on the RHS
    /// of a `|` pipe operator. The pipe implicitly supplies the LHS as
    /// the call's first positional argument, so the static arity /
    /// type check would false-flag (`range(5) | len()` would otherwise
    /// flag `len()` as missing its 1 arg). The walker pre-collects
    /// these ids before recursing so the FnCall arm can suppress the
    /// signature check on them.
    pipe_target_calls: HashSet<relon_parser::NodeId>,
    /// R1 (contextual closure typing): when a closure literal appears as
    /// an argument to a call/method whose resolved signature pins a
    /// `Closure<…>` slot at that position, we derive the closure's
    /// parameter types from the call context (unifying the signature's
    /// generics off the *other* args / receiver) and record them here,
    /// keyed by the closure node's id. The `Expr::Closure` arm consults
    /// this table so an otherwise-untyped param whose type IS derivable
    /// no longer trips `ClosureParamTypeMissing` under strict mode. Each
    /// entry is `param_name → TypeNode`; only the params that context
    /// could pin appear, so a genuinely unbindable param still falls
    /// through to the strict guard.
    closure_param_context: std::collections::HashMap<
        relon_parser::NodeId,
        std::collections::HashMap<String, relon_parser::TypeNode>,
    >,
}

impl<'a> Walker<'a> {
    fn visit(&mut self, node: &Node) {
        self.visit_internal(node, None);
    }

    fn visit_internal(&mut self, node: &Node, field_name: Option<&str>) {
        // Type binding check first — `Type field: value` carries info
        // that lets us validate `value` immediately, before recursing.
        if let Some(t) = &node.type_hint {
            self.check_typed_binding(t, node, field_name.unwrap_or("_"));

            // If the type-hint is a custom schema, also walk the value's
            // dict fields against the schema's expected types.
            self.check_against_custom_schema(t, node);

            // v1.6: ban `Any` anywhere in the typed-binding annotation.
            // Catches both bare `Any field: ...` and nested
            // `List<Any> xs: ...` forms.
            let mut to_emit: Vec<Diagnostic> = Vec::new();
            crate::ban_unsafe_types::scan_typenode_for_any(
                t,
                &format!("typed binding `{}`", field_name.unwrap_or("_")),
                &mut to_emit,
            );
            self.tree.diagnostics.extend(to_emit);
        }

        match &*node.expr {
            Expr::Dict(pairs) => {
                // v1.4: pre-register every sibling closure's signature
                // *before* `check_dict_v1_3` runs so typed-spread
                // resolution (`...sibling_call()`) can look up the
                // declared return type. Without this pre-pass the
                // signature wouldn't be visible until the closure-arm
                // of `visit_internal` fires later in the iteration.
                for (key, value) in pairs {
                    if let (
                        TokenKey::String(name, _, _),
                        Expr::Closure {
                            params,
                            return_type,
                            body,
                        },
                    ) = (key, &*value.expr)
                    {
                        self.tree
                            .field_closure_index
                            .insert(name.to_string(), value.id);
                        let sig = extract_closure_signature(value, params, return_type, body);
                        self.tree.closure_signatures.insert(value.id, sig);
                    }
                }
                // v1.3: validate spread / dynamic-key typehints under
                // strict mode, and surface DuplicateField clashes at
                // every mode (strict-or-not). The check runs *before*
                // we recurse so a typed spread that contributes a
                // schema's keys to the frame's static set still affects
                // the visible field set seen by the children.
                self.check_dict_v1_3(pairs);
                let frame = build_frame(pairs);
                self.scope_stack.push(frame);
                for (key, value) in pairs {
                    let field_name = if let TokenKey::String(name, _, _) = key {
                        Some(name.as_str())
                    } else {
                        None
                    };
                    // Stage 3.3: when a dict field's value is itself a
                    // closure, index the field name → closure NodeId so
                    // sibling-callable lookups (`{ f(x): x, y: f(1) }`)
                    // can find the static signature without re-walking.
                    // Idempotent re-insert here keeps the original
                    // Stage 3.3 invariant for shapes that don't go
                    // through the v1.4 pre-pass (no String key).
                    if let (Some(name), Expr::Closure { .. }) = (field_name, &*value.expr) {
                        self.tree
                            .field_closure_index
                            .insert(name.to_string(), value.id);
                    }
                    // v1.5: under strict mode every untyped dict
                    // value must produce a derivable type. Skip
                    // typed slots (the typed-binding walker already
                    // owns those) and references / variables (their
                    // own arms route through `check_strict_path`).
                    if self.tree.strict_mode
                        && value.type_hint.is_none()
                        && matches!(key, TokenKey::String(_, _, _))
                        && !matches!(
                            &*value.expr,
                            Expr::Variable(_)
                                | Expr::Reference { .. }
                                | Expr::Closure { .. }
                                | Expr::Dict(_)
                                | Expr::List(_)
                        )
                    {
                        let any_result = {
                            let scope = self.build_type_scope();
                            let t = infer_type(value, &scope).unwrap_or(InferredType::Any);
                            matches!(t, InferredType::Any)
                        };
                        if any_result {
                            self.tree
                                .diagnostics
                                .push(Diagnostic::ExpressionTypeUnknown {
                                    reason: format!(
                                        "dict field `{}` value type is not statically derivable",
                                        field_name.unwrap_or("_")
                                    ),
                                    range: span_of(value.range),
                                });
                        }
                    }
                    self.visit_internal(value, field_name);
                }
                self.scope_stack.pop();
            }
            Expr::Closure {
                params,
                body,
                return_type,
            } => {
                // R1: contextual param types derived from the enclosing
                // call site (if any), keyed by this closure node's id.
                // An untyped param that context could pin reads its type
                // from here; the strict guard then skips it.
                let ctx_param_types = self.closure_param_context.get(&node.id).cloned();
                let mut frame = ScopeFrame::default();
                for param in params {
                    frame.closure_params.insert(param.name.clone(), body.id);
                    if let Some(t) = &param.type_hint {
                        frame
                            .closure_param_types
                            .insert(param.name.clone(), t.clone());
                    } else if let Some(t) =
                        ctx_param_types.as_ref().and_then(|m| m.get(&param.name))
                    {
                        // R1: seed the contextually-derived type so the
                        // body's inference scope resolves `Variable(x)`
                        // heads to the pinned type — same slot the
                        // explicit annotation would fill.
                        frame
                            .closure_param_types
                            .insert(param.name.clone(), t.clone());
                    }
                }
                // v1.5: under strict mode every closure parameter must
                // declare a type — an untyped param defaults to `Any`,
                // which leaks an unclassified type into the body's
                // inference scope. Pin the diagnostic on the closure's
                // own range (the param spans aren't on `ClosureParam`).
                //
                // R1: a param whose type the enclosing call pins
                // contextually is *not* an `Any` leak — its type is
                // derivable, so suppress the diagnostic for it. Params
                // that context could not pin still fire (the strict guard
                // is narrowed, not removed).
                if self.tree.strict_mode {
                    for param in params {
                        if param.type_hint.is_some() {
                            continue;
                        }
                        let pinned_by_context = ctx_param_types
                            .as_ref()
                            .map(|m| m.contains_key(&param.name))
                            .unwrap_or(false);
                        if pinned_by_context {
                            continue;
                        }
                        self.tree
                            .diagnostics
                            .push(Diagnostic::ClosureParamTypeMissing {
                                param_name: param.name.clone(),
                                range: span_of(node.range),
                            });
                    }
                }
                // v1.6: ban `Any` (recursively) in closure param types
                // and the optional `-> ReturnType` annotation. Fires
                // in *every* mode; strict and non-strict alike are
                // expected to drop `Any` from user code.
                {
                    let mut to_emit: Vec<Diagnostic> = Vec::new();
                    for param in params {
                        if let Some(t) = &param.type_hint {
                            crate::ban_unsafe_types::scan_typenode_for_any(
                                t,
                                &format!("closure parameter `{}`", param.name),
                                &mut to_emit,
                            );
                        }
                    }
                    if let Some(rt) = return_type {
                        crate::ban_unsafe_types::scan_typenode_for_any(
                            rt,
                            "closure return type",
                            &mut to_emit,
                        );
                    }
                    self.tree.diagnostics.extend(to_emit);
                }
                // Stage 3.3: extract the closure's signature *before*
                // walking the body so any nested FnCall to a sibling-
                // bound recursive closure resolves against the same
                // signature. The walker doesn't carry the closure's own
                // NodeId (it's the parent node's id), so we record it
                // against the field-value node id supplied by the dict
                // arm via `field_closure_index`.
                let sig = extract_closure_signature(node, params, return_type, body);
                self.tree.closure_signatures.insert(node.id, sig);
                self.scope_stack.push(frame);
                if let Some(declared_return) = return_type {
                    self.check_closure_return(params, body, declared_return, field_name);
                } else if self.tree.strict_mode {
                    // No declared `-> ReturnType` — the closure's
                    // signature falls back to the body's inferred
                    // type. Strict mode demands that body inference
                    // succeed; if it lands on `Any` we can't
                    // distinguish a typed-`Any` slot from a real
                    // failure, so push a closure-body diagnostic.
                    //
                    // R1: `frame` (already pushed onto the scope stack)
                    // carries both explicit and contextually-derived
                    // closure-param types, so `build_type_scope` resolves
                    // `Variable(x)` heads to the pinned types and the
                    // body infers concretely — no false
                    // `ClosureReturnTypeUnknown` when the type IS
                    // derivable.
                    let scope = self.build_type_scope();
                    let body_ty = infer_type(body, &scope).unwrap_or(InferredType::Any);
                    if matches!(body_ty, InferredType::Any) {
                        self.tree
                            .diagnostics
                            .push(Diagnostic::ClosureReturnTypeUnknown {
                                role: field_name.unwrap_or("<closure>").to_string(),
                                range: span_of(body.range),
                            });
                    }
                }
                self.visit_internal(body, None);
                self.scope_stack.pop();
            }
            Expr::List(items) => {
                // v1.5: under strict mode, every list element must
                // produce a derivable type. We collect diagnostics
                // into a local vec so the inference scope's read-only
                // borrow of `self.tree` stays disjoint from the push.
                if self.tree.strict_mode {
                    let mut to_emit: Vec<Diagnostic> = Vec::new();
                    {
                        let scope = self.build_type_scope();
                        for item in items {
                            // Skip type-hinted items — the typed-
                            // binding pass already covers their slot.
                            if item.type_hint.is_some() {
                                continue;
                            }
                            let t = infer_type(item, &scope).unwrap_or(InferredType::Any);
                            if matches!(t, InferredType::Any)
                                && !matches!(
                                    &*item.expr,
                                    Expr::Variable(_) | Expr::Reference { .. }
                                )
                            {
                                to_emit.push(Diagnostic::ExpressionTypeUnknown {
                                    reason: "list element type is not statically derivable"
                                        .to_string(),
                                    range: span_of(item.range),
                                });
                            }
                        }
                    }
                    self.tree.diagnostics.extend(to_emit);
                }
                for item in items {
                    self.visit_internal(item, None);
                }
            }
            Expr::Reference { base, path } => {
                self.check_unresolved_ref(node, base, path);
            }
            Expr::Variable(path) => {
                self.check_unresolved_var(node, path);
                // Schema-rooted §J follow-up: a Dynamic segment in a
                // path lowers to `receiver.index(key)`. When the
                // receiver schema declares a concrete `index(key:
                // K)` parameter type, validate the dynamic key
                // expression's static type matches; otherwise the
                // mismatch would only surface at runtime from inside
                // the witness body.
                self.check_index_dispatch(path);
            }
            Expr::Match { expr, arms } => {
                self.check_match_exhaustiveness(node.range, expr, arms);
                self.check_match_arm_types(node.range, expr, arms);
                self.visit_internal(expr, None);
                for (pat, body) in arms {
                    self.visit_internal(pat, None);
                    self.visit_internal(body, None);
                }
            }
            Expr::Binary(op, left, right) => {
                self.check_binary_mismatch(node, *op, left, right, field_name);
                // Stage 5: literal-only arithmetic — divide-by-zero
                // and i64 overflow get surfaced statically. When the
                // outer node folds to an error we *don't* recurse,
                // because the same diagnostic would re-fire on every
                // failing intermediate sub-node (`(1+2)*(3+4)/0` →
                // both `0/0`'s parent and the outer node would
                // double-report). Successful or non-foldable nodes
                // still walk their children so we don't drop any
                // sibling-arithmetic errors hidden under a non-
                // arithmetic head.
                if !self.check_const_fold(node) {
                    self.visit_internal(left, None);
                    self.visit_internal(right, None);
                }
            }
            Expr::Unary(_, inner) => {
                // Stage 5: same treatment as Binary — `-i64::MIN`
                // overflows at fold time and we shouldn't continue
                // down into the literal child once we've reported.
                if !self.check_const_fold(node) {
                    self.visit_internal(inner, None);
                }
            }
            Expr::FnCall { path, args } => {
                self.check_unresolved_fn_call(node, path);
                self.check_fn_call(node, path, args);
                self.check_strict_fn_call(node, path);
                self.check_method_dispatch(node, path);
                // R1: derive contextual closure-param types from this
                // call's resolved signature *before* recursing, so the
                // `Expr::Closure` arm sees the pinned types when it
                // decides whether to fire `ClosureParamTypeMissing`.
                self.populate_closure_context(path, args);
                for arg in args {
                    self.visit_internal(&arg.value, None);
                }
            }
            Expr::Where { expr, bindings } => {
                // Phase 9.b-3: mirror the resolve.rs walker so the body
                // expression's strict-mode path walker (`check_strict_path`
                // → `TypeScope::lookup`) sees where-bound names through
                // `scope_stack`. Without the push the body's references
                // miss the binding frame and `check_strict_path`
                // false-positives `UnknownReferenceType` on every
                // where-bound name under `#strict`.
                self.visit_internal(bindings, None);
                if let Expr::Dict(pairs) = &*bindings.expr {
                    let frame = build_frame(pairs);
                    self.scope_stack.push(frame);
                    self.visit_internal(expr, None);
                    self.scope_stack.pop();
                } else {
                    self.visit_internal(expr, None);
                }
            }
            Expr::Comprehension {
                element,
                id,
                iterable,
                condition,
            } => {
                // R1: `[ body for x in <iterable> ]` binds `x` to the
                // iterable's element type. We infer the iterable's type,
                // peel its element type (`List<T>` → `T`, `range(...)` →
                // `Int`, list literal `Tuple(..)` → join), and push a
                // scope frame whose `closure_param_types` carries `x`.
                // The element / condition bodies then infer `Variable(x)`
                // concretely instead of tripping `UnknownReferenceType`.
                //
                // The iterable is checked in the *outer* scope (the
                // binding isn't visible to its own source).
                self.visit_internal(iterable, None);

                let elem_ty = {
                    let scope = self.build_type_scope();
                    let iter_ty = infer_type(iterable, &scope).unwrap_or(InferredType::Any);
                    match iter_ty {
                        InferredType::List(t) => Some(*t),
                        InferredType::Dict(v) => Some(*v),
                        InferredType::Tuple(elems) if !elems.is_empty() => elems
                            .into_iter()
                            .reduce(|acc, t| InferredType::join(&acc, &t)),
                        _ => None,
                    }
                };

                let mut frame = ScopeFrame::default();
                frame.closure_params.insert(id.clone(), element.id);
                // Only pin the binding's type when it derived to
                // something concrete.
                let pinned = match &elem_ty {
                    Some(t) if !matches!(t, InferredType::Any) => {
                        let tn = crate::generics::type_node_for(t);
                        frame.closure_param_types.insert(id.clone(), tn);
                        true
                    }
                    _ => false,
                };
                // R1: keep rejecting true `Any` leaks. When the iterable's
                // element type isn't derivable (e.g. iterating a scalar,
                // or an opaque value), strict mode must not silently
                // accept the comprehension — the binding `x` would leak
                // `Any` into the body. The `closure_params` entry above
                // would otherwise let `check_strict_path` dedup-suppress
                // the body reference (it can't tell a comprehension
                // binding from a closure param), so we surface the failure
                // here on the iterable itself.
                if !pinned && self.tree.strict_mode {
                    self.tree
                        .diagnostics
                        .push(Diagnostic::ExpressionTypeUnknown {
                            reason: format!(
                                "comprehension binding `{id}` has no derivable element type \
                                 (the iterable's element type is not statically known)"
                            ),
                            range: span_of(iterable.range),
                        });
                }
                self.scope_stack.push(frame);
                self.visit_internal(element, None);
                if let Some(c) = condition {
                    self.visit_internal(c, None);
                }
                self.scope_stack.pop();
            }
            _ => {
                for child in child_nodes(node) {
                    self.visit_internal(child, None);
                }
            }
        }
    }
}

// `static_type_of` / `matches_expected` were the pre-Stage-1.2
// String-name inference helpers. They've been fully replaced by the
// `infer::infer_type` engine and `InferredType::subsumes`, and the
// last in-tree caller migrated in Stage 1.4. The legacy helpers are
// removed here; consumers that need the old name-string view can
// build it from `InferredType::name`.
