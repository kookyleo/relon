//! `#main(...) -> Type` return-type pre-flight check.
//!
//! When the entry program declares an explicit return type, run the
//! inference engine over the body and compare. This is the analyzer's
//! parallel to the evaluator's runtime
//! `RuntimeError::MainReturnTypeMismatch` — the runtime check still
//! catches dynamic mismatches that depend on host-pushed values, but
//! purely-static mismatches now surface here so callers don't even
//! enter evaluation.

use crate::diagnostic::{span_of, Diagnostic};
use crate::infer::{infer_from_type_node_with_imports, infer_type, InferredType, TypeScope};
use crate::tree::AnalyzedTree;
use crate::typecheck::format_type;
use relon_parser::{Node, TypeNode};
use std::collections::HashMap;

/// Walk the entry root's body under the inferred-types lens and push a
/// [`Diagnostic::MainReturnTypeMismatch`] if the body's static type
/// disagrees with the `#main` return-type annotation. Does nothing when
/// there is no annotation (the entry's return value is unchecked) or
/// when the body's type cannot be inferred (call into stdlib, dynamic
/// reference, …).
///
/// v1.3: the scope is seeded with every `#main(...)` parameter so an
/// atomic / arithmetic root body that references a param (e.g.
/// `#main(Int n) -> String\nn+1`) can reach the param's declared type
/// during inference rather than collapsing to `Any` and silently
/// passing the return-type check.
pub(crate) fn check_main_return(root: &Node, tree: &mut AnalyzedTree) {
    let Some(signature) = tree.main_signature.as_ref() else {
        return;
    };
    let Some(return_type) = signature.return_type.as_ref() else {
        return;
    };
    // Build a scope rooted at the document so `Variable(x)` heads in
    // the body can find their typed siblings.
    let schemas = crate::typecheck::build_schema_index(tree);
    let bases = crate::typecheck::build_base_index(tree);
    let mut scope = TypeScope::new(tree, &schemas);
    let imports = tree.workspace_import_index.as_ref();
    for param in &signature.params {
        scope.locals.insert(
            param.name.clone(),
            infer_from_type_node_with_imports(&param.type_node, imports),
        );
    }
    let Some(body_ty) = infer_type(root, &scope) else {
        return;
    };
    // When the body's inference collapsed to `Any` we cannot prove a
    // mismatch (no `MainReturnTypeMismatch`), but the declared
    // `-> Type` annotation then goes entirely unverified. `Any`
    // subsumes every slot, so this must be handled *before* the
    // subsumption check or it silently passes. Under strict mode the
    // information gap surfaces (same class as `ExpressionTypeUnknown`
    // elsewhere); `#relaxed` keeps the runtime-check fallback silent.
    if matches!(body_ty, InferredType::Any) {
        if tree.strict_mode {
            tree.diagnostics.push(Diagnostic::ExpressionTypeUnknown {
                reason: format!(
                    "entry body inferred `Any`, so the declared `#main` return type `{}` cannot be verified statically",
                    format_type(return_type)
                ),
                range: span_of(signature.range),
            });
        }
        return;
    }
    if tuple_schema_return_matches(tree, return_type, &body_ty, &bases) {
        return;
    }
    // A top-level `Any` #main body is already handled above (strict
    // emits `ExpressionTypeUnknown`). The #main return value is
    // runtime-checked against the declared type, so this subsumption
    // keeps the permissive `Any` pass (non-strict) — the fail-closed
    // gate is scoped to the function-argument boundary in `fn_call.rs`.
    if body_ty.subsumes_with_imports(
        return_type,
        Some(&bases),
        tree.workspace_import_index.as_ref(),
        false,
    ) {
        return;
    }
    tree.diagnostics.push(Diagnostic::MainReturnTypeMismatch {
        expected: format_type(return_type),
        found: body_ty.name(),
        range: span_of(signature.range),
    });
}

fn tuple_schema_return_matches(
    tree: &AnalyzedTree,
    return_type: &TypeNode,
    body_ty: &InferredType,
    bases: &crate::infer::SchemaBaseIndex,
) -> bool {
    let Some((schema_name, mut elements)) =
        crate::schema::tuple_elements_for_schema_type(tree, return_type)
    else {
        return false;
    };
    let InferredType::Tuple(items) = body_ty else {
        return false;
    };
    if items.len() != elements.len() {
        return false;
    }
    let subst = tuple_schema_generic_subst(tree, &schema_name, return_type);
    if !subst.is_empty() {
        elements = elements
            .iter()
            .map(|t| crate::typecheck::substitute_generics_in_typenode(t, &subst))
            .collect();
    }
    items.iter().zip(elements.iter()).all(|(item, slot)| {
        item.subsumes_with_imports(
            slot,
            Some(bases),
            tree.workspace_import_index.as_ref(),
            false,
        )
    })
}

fn tuple_schema_generic_subst(
    tree: &AnalyzedTree,
    schema_name: &str,
    expected: &TypeNode,
) -> HashMap<String, TypeNode> {
    if expected.generics.is_empty() {
        return HashMap::new();
    }
    let params = tree
        .schemas
        .values()
        .find(|def| def.name.as_deref() == Some(schema_name))
        .map(|def| def.generics.clone())
        .or_else(|| {
            tree.root_schemas
                .iter()
                .find(|d| d.name == schema_name)
                .map(|d| d.generics.clone())
        })
        .or_else(|| {
            tree.workspace_import_index
                .as_ref()
                .and_then(|idx| idx.imported_schema_generics.get(schema_name).cloned())
        })
        .unwrap_or_default();
    params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| expected.generics.get(i).map(|arg| (p.clone(), arg.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    fn analyze_str(src: &str) -> AnalyzedTree {
        let node = parse_document(src).unwrap();
        analyze(&node)
    }

    /// Forward: body produces a Dict but `#main` declares String.
    #[test]
    fn flags_main_return_dict_vs_string() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> String
            { result: n + 1 }
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
    }

    /// v1.3a forward: atomic root body that uses a `#main` param. The
    /// param injection at the resolve / typecheck level makes the
    /// inferred body type `Int` (matching `Int + Int`); the declared
    /// return is `String`, so the analyzer must flag a static
    /// `MainReturnTypeMismatch` rather than letting it slip through to
    /// runtime as it did in v1.2.
    #[test]
    fn v1_3a_flags_atomic_root_main_param_return_mismatch() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> String
            n + 1
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
                if expected == "String" && found == "Int")
            })
            .collect();
        assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
    }

    /// v1.3a reverse: the atomic root body's type matches the declared
    /// return type — silent.
    #[test]
    fn v1_3a_atomic_root_main_param_matching_return() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> Int
            n + 1
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Reverse: body matches the declared Dict return type — silent.
    #[test]
    fn does_not_flag_matching_main_return() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> Dict
            { result: n + 1 }
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.3a: boundary — multiple `#main` params; each is independently
    /// resolvable in the root body.
    #[test]
    fn v1_3a_resolves_multiple_main_params() {
        let tree = analyze_str(
            r#"
            #main(Int n, String s) -> String
            s
            "#,
        );
        // Body type `String` matches declared return — no
        // MainReturnTypeMismatch.
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
        // No UnresolvedReference for `s`.
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "s"))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.3a: boundary — list root that uses params produces matching
    /// `List<Int>` type.
    #[test]
    fn v1_3a_list_root_with_param_matches_return() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> List<Int>
            [n, n + 1, n * 2]
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.3a: boundary — `#main(Int n) -> Float` with `n + 1` should
    /// flag because `Int + Int = Int` (not Float).
    #[test]
    fn v1_3a_atomic_int_against_float_return_mismatch() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> Float
            n + 1
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Reverse: body type is uninferrable (custom schema we don't
    /// model) — silent, fall back to runtime.
    #[test]
    fn does_not_flag_uninferrable_main_return() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> SomeSchema
            { result: range(0, n) }
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    // ====== v1.4 path-tail walking ======

    /// v1.4 forward: `#main(Order o) -> Int` with body `o.id` (where
    /// `Order { Int id: * }`) infers the body as `Int`, matching the
    /// declared return — no diagnostic. Before v1.4 the `Variable("o")`
    /// resolved to `Schema(Order)` but the path tail `.id` was dropped,
    /// leaving the body type at `Schema(Order)` (which would have
    /// false-flagged against `Int`).
    #[test]
    fn v1_4_path_tail_atomic_field_matches_int_return() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, Float total: * }
            #main(Order o) -> Int
            o.id
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: same body as the previous case but the declared
    /// return is `String` — a static `MainReturnTypeMismatch` should
    /// fire. Before v1.4 the analyzer collapsed the body to `Any` and
    /// swallowed the mismatch.
    #[test]
    fn v1_4_path_tail_atomic_field_string_return_mismatch() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, Float total: * }
            #main(Order o) -> String
            o.id
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
                    if expected == "String" && found == "Int")
            })
            .collect();
        assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: multi-hop schema path. `Customer.name : String` is
    /// reachable via `o.customer.name`, and the body's inferred type
    /// should match the declared `String` return.
    #[test]
    fn v1_4_path_tail_multi_hop_schema_chain() {
        let tree = analyze_str(
            r#"
            #schema Customer { String name: * }
            #schema Order { Customer customer: *, Int id: * }
            #main(Order o) -> String
            o.customer.name
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: `Dict<String, Int>` head then a string-keyed step
    /// produces `Int`, satisfying the declared return.
    #[test]
    fn v1_4_path_tail_dict_value_chain() {
        let tree = analyze_str(
            r#"
            #main(Dict<String, Int> kv) -> Int
            kv.foo
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Strict: a body that collapses to `Any` leaves the declared
    /// `-> Type` annotation unverifiable — the information gap must
    /// surface as `ExpressionTypeUnknown` instead of silently passing.
    #[test]
    fn strict_flags_unverifiable_main_return_on_any_body() {
        // A path step into an Int yields `UnknownStep`, which the
        // inference wrapper collapses to `Any` — previously the
        // `Any`-subsumes-everything shortcut let the unverified
        // `-> String` annotation pass silently even in strict mode.
        let tree = analyze_str(
            r#"
            #main(Int n) -> String
            n.something
            "#,
        );
        let gap: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
                    if reason.contains("#main") && reason.contains("String"))
            })
            .collect();
        assert_eq!(gap.len(), 1, "{:?}", tree.diagnostics);
        // Still no false mismatch claim.
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// `#relaxed`: the same `Any`-body entry stays silent — the
    /// return-type check falls back to the runtime verdict.
    #[test]
    fn relaxed_keeps_unverifiable_main_return_silent() {
        let tree = analyze_str(
            r#"
            #relaxed
            #main(Int n) -> String
            n.something
            "#,
        );
        let gap: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
                    if reason.contains("#main"))
            })
            .collect();
        assert!(gap.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Strict reverse: an inferrable body does not trigger the
    /// information-gap diagnostic from the return-type pass.
    #[test]
    fn strict_inferrable_body_no_main_return_gap() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> Int
            n + 1
            "#,
        );
        let gap: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
                    if reason.contains("#main"))
            })
            .collect();
        assert!(gap.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.4 reverse: stepping into a non-schema, non-dict head (Int has
    /// no fields). Non-strict mode: the inference falls back to `Any` /
    /// silent. We still allow the walker to swallow the mismatch (no
    /// `MainReturnTypeMismatch` in non-strict). Strict-mode handling is
    /// covered by the `strict_silent_fallback` fixtures.
    #[test]
    fn v1_4_path_tail_int_descend_silent_non_strict() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> String
            n.something
            "#,
        );
        // Body's path-tail walk fails at `n.something` — non-strict
        // falls back to Any; the return-type checker skips the
        // `Any`-body case, so no MainReturnTypeMismatch.
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    // ============= v1.5: comprehension / where in entry body =============

    /// v1.5 forward: comprehension as the entry's body. Element type
    /// derives to Int via the binding scope; the result `List<Int>`
    /// matches the declared return — no diagnostic.
    #[test]
    fn v1_5_main_return_comprehension_match() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> List<Int>
            [x * x for x in range(n)]
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: comprehension whose element type doesn't match
    /// the declared return — surfaced statically.
    #[test]
    fn v1_5_main_return_comprehension_mismatch() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> List<String>
            [x * x for x in range(n)]
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: where-expression as the entry body. The body
    /// `(n + 1)` infers Int when `n: x` and `x: Int`.
    #[test]
    fn v1_5_main_return_where_match() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> Int
            (n + 1) where { n: x }
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: where-body Int leaks against String return.
    #[test]
    fn v1_5_main_return_where_mismatch() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> String
            (n + 1) where { n: x }
            "#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
                if expected == "String" && found == "Int")
            })
            .collect();
        assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
    }
}
