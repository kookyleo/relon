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
use crate::infer::{infer_type, InferredType, TypeScope};
use crate::tree::AnalyzedTree;
use relon_parser::Node;

/// Walk the entry root's body under the inferred-types lens and push a
/// [`Diagnostic::MainReturnTypeMismatch`] if the body's static type
/// disagrees with the `#main` return-type annotation. Does nothing when
/// there is no annotation (the entry's return value is unchecked) or
/// when the body's type cannot be inferred (call into stdlib, dynamic
/// reference, …).
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
    let scope = TypeScope::new(tree, &schemas);
    let Some(body_ty) = infer_type(root, &scope) else {
        return;
    };
    if body_ty.subsumes_with(return_type, Some(&bases)) {
        return;
    }
    // Avoid double-reporting when the body's inference already
    // collapsed to `Any` — we'd just be repeating the runtime's
    // verdict.
    if matches!(body_ty, InferredType::Any) {
        return;
    }
    tree.diagnostics.push(Diagnostic::MainReturnTypeMismatch {
        expected: format_type(return_type),
        found: body_ty.name(),
        range: span_of(signature.range),
    });
}

fn format_type(t: &relon_parser::TypeNode) -> String {
    let suffix = if t.is_optional { "?" } else { "" };
    let path = t.path.join(".");
    if t.generics.is_empty() {
        format!("{path}{suffix}")
    } else {
        let inner: Vec<String> = t.generics.iter().map(format_type).collect();
        format!("{path}<{}>{suffix}", inner.join(", "))
    }
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
}
