//! v1.1: generic-instantiation helpers for the FnCall checker.
//!
//! Stage 3's `FnSignature` is monomorphic — every polymorphic stdlib
//! fn (`_list_map`, `_list_filter`, …) widens its return slot to `Any`,
//! losing element-type information. v1.1 adds a `generics: Vec<String>`
//! field to [`crate::sig::FnSignature`] and uses this module to:
//!
//! 1. Walk each declared param `TypeNode` in lock-step with the actual
//!    arg's [`InferredType`], collecting bindings (`T → Int`,
//!    `U → Bool`, …).
//! 2. Apply the resulting `HashMap<String, TypeNode>` back to the
//!    signature's params, variadic_tail, and return_type via
//!    [`crate::sig::instantiate`].
//!
//! The algorithm is deliberately simple — no occurs-check, no
//! higher-rank quantification, no row polymorphism. It's just enough
//! to thread `T` through `List<T>` / `Closure<(T) -> U>` for the
//! standard list pipeline.
//!
//! ### Corner cases
//!
//! * **Arg type unknown.** When `infer_type` returns `None` (FnCall
//!   without a static signature, dynamic spread, …) we skip that arg
//!   for binding purposes — every `T` it would have nailed down stays
//!   unbound. The substitution step leaves the residual placeholder
//!   in place; downstream subsumption sees a single-segment custom
//!   path it doesn't recognize, falls into the "any non-builtin head"
//!   accept-all branch, and silently passes. So an unbindable arg
//!   degrades back to "no extra information", matching v1's behavior.
//!
//! * **Arg is `Any`.** Same as above, but explicit: we don't bind
//!   `T → Any` because that would lock the placeholder to a useless
//!   value. Leaving `T` unbound preserves whatever later args manage
//!   to pin down.
//!
//! * **Conflicting binds.** If two arg slots both bind `T` but to
//!   different types (`T → Int` then `T → String`), we widen via
//!   [`crate::infer::InferredType::join`]. When the join collapses to
//!   `Any` (no useful upper bound), the placeholder is treated as
//!   unbound — same degradation path as the previous bullets.

use crate::infer::{infer_from_type_node, infer_type, InferredType, TypeScope};
use crate::sig::{type_node_simple, FnSignature};
use relon_parser::{CallArg, Expr, TypeNode};
use std::collections::HashMap;

/// Walk `param_ty` and `arg_ty` in parallel, recording any generic
/// placeholder → concrete type binding. `generics` lists the names
/// that count as placeholders in this signature; everything else is a
/// concrete head and contributes to neither the binding map nor the
/// caller's diagnostics (the per-arg subsumption check already owns
/// type-mismatch reporting).
///
/// Returns silently when no binding can be made (e.g. arg is `Any`,
/// or the shapes don't line up). v1 unification is best-effort by
/// design — failure here is *not* a hard error; the caller still runs
/// the regular subsumption check on the substituted param ty.
pub(crate) fn unify(
    param_ty: &TypeNode,
    arg_ty: &InferredType,
    generics: &[String],
    bindings: &mut HashMap<String, TypeNode>,
) {
    // Single-segment, zero-generic placeholder — bind it.
    if param_ty.path.len() == 1
        && param_ty.generics.is_empty()
        && generics.iter().any(|g| g == &param_ty.path[0])
    {
        let key = param_ty.path[0].clone();
        // Don't bind to `Any` — leaving the placeholder unbound is
        // strictly more informative than locking it to `Any`.
        if matches!(arg_ty, InferredType::Any) {
            return;
        }
        let new_ty = type_node_for(arg_ty);
        bindings
            .entry(key)
            .and_modify(|prev| {
                let prev_inf = infer_from_type_node(prev);
                let joined = InferredType::join(&prev_inf, arg_ty);
                if !matches!(joined, InferredType::Any) {
                    *prev = type_node_for(&joined);
                }
                // Otherwise keep `prev` — joining to `Any` is a
                // conflict; widening would erase information already
                // collected. Caller's subsumption check still flags
                // the mismatch downstream.
            })
            .or_insert(new_ty);
        return;
    }
    // Concrete head — recurse into matching generic slots when both
    // sides agree on the head. Mismatched heads are silently ignored
    // (the per-arg subsumption check handles diagnostics).
    if param_ty.path.len() != 1 {
        return;
    }
    let head = param_ty.path[0].as_str();
    match (head, arg_ty) {
        ("List", InferredType::List(elem)) => {
            if let Some(inner) = param_ty.generics.first() {
                unify(inner, elem, generics, bindings);
            }
        }
        ("Dict", InferredType::Dict(val)) => {
            // Dict<K, V>: keys are always String in the language, so
            // only the value slot can carry a placeholder we'd want
            // to bind.
            if let Some(v_slot) = param_ty.generics.get(1) {
                unify(v_slot, val, generics, bindings);
            }
        }
        ("Closure" | "Fn", InferredType::Fn(arg_params, arg_ret)) => {
            // Match Closure<(T1, T2) -> U> param-by-param. The
            // language doesn't currently surface a structured fn-type
            // syntax in source, so this branch is here mostly for
            // forward compatibility — today's stdlib param slots use
            // `Any` for closures and rely on the body's inferred fn
            // type during the per-arg subsumption check.
            for (slot, t) in param_ty.generics.iter().zip(arg_params.iter()) {
                unify(slot, t, generics, bindings);
            }
            if let Some(ret_slot) = param_ty.generics.get(arg_params.len()) {
                unify(ret_slot, arg_ret, generics, bindings);
            }
        }
        _ => {}
    }
}

/// Best-effort lift from [`InferredType`] back into a concrete
/// [`TypeNode`]. We can always represent the simple builtin / list /
/// dict / schema shapes; complex shapes (variants, optional unions
/// inside generics, fn types) collapse to `Any`. The cases that
/// matter for v1.1's stdlib pipeline (`Int`, `String`, `Bool`,
/// `List<X>`, `Schema(name)`) all round-trip cleanly.
///
/// `pub(crate)` so the R1 comprehension-binding typing in the
/// type-check walker can reuse the same lift when it pins the
/// iteration variable's derived element type.
pub(crate) fn type_node_for(t: &InferredType) -> TypeNode {
    use crate::sig::{type_node_generic, type_node_simple};
    match t {
        InferredType::Any => type_node_simple("Any"),
        InferredType::Null => type_node_simple("Null"),
        InferredType::Bool => type_node_simple("Bool"),
        InferredType::Int => type_node_simple("Int"),
        InferredType::Float => type_node_simple("Float"),
        InferredType::Number => type_node_simple("Number"),
        InferredType::String => type_node_simple("String"),
        InferredType::List(inner) => type_node_generic("List", vec![type_node_for(inner)]),
        InferredType::Dict(val) => {
            type_node_generic("Dict", vec![type_node_simple("String"), type_node_for(val)])
        }
        InferredType::Schema(name) => type_node_simple(name),
        InferredType::Variant(enum_name, _) => type_node_simple(enum_name),
        InferredType::Optional(inner) => {
            let mut node = type_node_for(inner);
            node.is_optional = true;
            node
        }
        // Fn / closure types don't have a stable surface syntax in
        // v1; collapse to `Any`. Round-trip back through
        // `infer_from_type_node` would also yield `Any`, so no
        // information is lost.
        InferredType::Fn(_, _) => type_node_simple("Any"),
        // v1.7: tuple round-trips through the `Tuple<T1, ...>`
        // single-segment encoding the parser uses for `(T1, ...)`.
        InferredType::Tuple(elems) => {
            let inner: Vec<TypeNode> = elems.iter().map(type_node_for).collect();
            type_node_generic("Tuple", inner)
        }
    }
}

/// v1.1 high-level: collect generic placeholder bindings for `sig` at
/// the call site `args`, returning a fully-populated map (every name
/// in `sig.generics` either bound from an arg or filled with `Any`).
///
/// Two-pass strategy so closure literals can pull their body type
/// back into binding-space:
///
/// 1. **Non-closure args** — infer each arg's type and unify against
///    the corresponding param `TypeNode`. This binds simple T's from
///    `List<T>`, `Dict<String, V>`, plain T slots, etc.
/// 2. **Closure-literal args** — for each arg whose source is a
///    `Closure { ... }` node, build a child [`TypeScope`] where the
///    closure's params get the types implied by the param's
///    `Closure<T1, ..., Ret>` slot under the partial bindings from
///    pass 1; infer the body's type; unify it against the `Ret`
///    slot. This is what lets `_list_map([1,2,3], (n) => n + 1)`
///    bind `U → Int` (closure body type) so the return slot reads
///    back as `List<Int>` instead of `List<Any>`.
///
/// Step 3 (Any-fallback): every `g` in `sig.generics` not bound by
/// the two passes is set to `Any` so downstream substitution leaves
/// no residual placeholder.
pub(crate) fn collect_bindings(
    sig: &FnSignature,
    args: &[CallArg],
    scope: &TypeScope,
) -> HashMap<String, TypeNode> {
    let mut bindings: HashMap<String, TypeNode> = HashMap::new();
    if sig.generics.is_empty() {
        return bindings;
    }

    // Pass 1: every non-closure-literal arg contributes its inferred
    // type to the unifier.
    for (idx, arg) in args.iter().enumerate() {
        if matches!(&*arg.value.expr, Expr::Closure { .. }) {
            continue;
        }
        let param_ty = match param_slot(sig, idx) {
            Some(p) => p,
            None => continue,
        };
        if let Some(arg_ty) = infer_type(&arg.value, scope) {
            unify(param_ty, &arg_ty, &sig.generics, &mut bindings);
        }
    }

    // Pass 2: closure-literal args. For each one whose param slot is
    // a `Closure<T1, ..., Ret>`, build a child TypeScope where the
    // closure params take their substituted types, infer the body,
    // and unify the body type against `Ret`.
    for (idx, arg) in args.iter().enumerate() {
        let Expr::Closure {
            params,
            return_type,
            body,
        } = &*arg.value.expr
        else {
            continue;
        };
        let Some(param_ty) = param_slot(sig, idx) else {
            continue;
        };
        if param_ty.path.len() != 1 {
            continue;
        }
        let head = param_ty.path[0].as_str();
        if head != "Closure" && head != "Fn" {
            continue;
        }
        // Slot positions: [param_tys.., return_ty]. A trailing slot
        // is required for the body-type binding to make sense.
        if param_ty.generics.is_empty() {
            continue;
        }
        let (slot_param_tys, slot_ret_ty) = param_ty.generics.split_at(param_ty.generics.len() - 1);
        let slot_ret_ty = &slot_ret_ty[0];

        // Build the child scope: closure params get either their
        // explicit annotation, or the slot type substituted under
        // current bindings (so `T` resolves to the type already
        // bound from pass 1). Use `child_with_locals` so the parent's
        // locals stay borrowed in-place rather than getting cloned
        // into the child map.
        let mut new_locals: HashMap<String, InferredType> = HashMap::with_capacity(params.len());
        let imports = scope.tree.and_then(|t| t.workspace_import_index.as_ref());
        for (p_idx, cp) in params.iter().enumerate() {
            let local_ty = if let Some(hint) = &cp.type_hint {
                crate::infer::infer_from_type_node_with_imports(hint, imports)
            } else if let Some(slot) = slot_param_tys.get(p_idx) {
                let mut sub = slot.clone();
                crate::sig::substitute_in_type_node(&mut sub, &sig.generics, &bindings);
                crate::infer::infer_from_type_node_with_imports(&sub, imports)
            } else {
                InferredType::Any
            };
            new_locals.insert(cp.name.clone(), local_ty);
        }
        let child_scope = scope.child_with_locals(new_locals);
        // Body type: prefer an explicit `-> Ret` annotation; fall
        // back to walking the body under the child scope.
        let body_ty = if let Some(rt) = return_type {
            crate::infer::infer_from_type_node_with_imports(rt, imports)
        } else {
            infer_type(body, &child_scope).unwrap_or(InferredType::Any)
        };
        unify(slot_ret_ty, &body_ty, &sig.generics, &mut bindings);
    }

    // Pass 3: fill any still-unbound generic with `Any` so
    // substitution downstream never leaves a residual placeholder.
    for g in &sig.generics {
        bindings
            .entry(g.clone())
            .or_insert_with(|| type_node_simple("Any"));
    }

    bindings
}

/// Pick the `TypeNode` slot for arg index `idx` — either the
/// declared positional param or the `variadic_tail`.
fn param_slot(sig: &FnSignature, idx: usize) -> Option<&TypeNode> {
    if idx < sig.params.len() {
        Some(&sig.params[idx].ty)
    } else {
        sig.variadic_tail.as_ref()
    }
}
