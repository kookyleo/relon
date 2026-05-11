//! Phase B integration tests for schema-rooted dispatch — the analyzer
//! lowers `with { ... }` blocks and `#extend` directives into a per-schema
//! method table, builds `FnSignature`s for each method, and resolves
//! `value.method(...)` / `Schema.method(...)` calls against that table.
//!
//! These tests pin the dispatch surface area (positive + negative) the
//! type-checker now owns. Body-level `Self` / `self` resolution and
//! evaluator dispatch land in a follow-up sub-phase.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

#[test]
fn simple_method_table_populated() {
    let tree = analyze_fixture("schema_methods/simple_method.relon");
    let methods = tree
        .schema_methods
        .get("Money")
        .expect("Money should appear in schema_methods");
    // Phase C.4 auto-derives `eq` and `to_json` onto every user
    // schema that hasn't opted out — filter them out to assert on the
    // user-written methods only.
    let user_methods: Vec<_> = methods.iter().filter(|m| !m.is_native).collect();
    assert_eq!(user_methods.len(), 1, "{:?}", tree.diagnostics);
    assert_eq!(user_methods[0].name, "cents_value");
    assert!(tree
        .method_signatures
        .contains_key(&("Money".to_string(), "cents_value".to_string())));
    let unknown = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownMethod { .. })
    });
    assert_eq!(unknown, 0, "{:?}", tree.diagnostics);
}

#[test]
fn unknown_method_diagnoses() {
    let tree = analyze_fixture("schema_methods/unknown_method.relon");
    let unknown = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownMethod { schema, method, .. }
            if schema == "Money" && method == "uknown_method")
    });
    assert_eq!(unknown, 1, "{:?}", tree.diagnostics);
}

#[test]
fn extend_on_user_schema() {
    let tree = analyze_fixture("schema_methods/extend_user_schema.relon");
    let methods = tree.schema_methods.get("User").expect("User extended");
    // Skip Phase C.4 auto-derived `eq` / `to_json` entries.
    let user_methods: Vec<_> = methods.iter().filter(|m| !m.is_native).collect();
    assert_eq!(user_methods.len(), 1);
    assert_eq!(user_methods[0].name, "is_admin");
    let no_unknown = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownMethod { .. })
    });
    assert_eq!(no_unknown, 0, "{:?}", tree.diagnostics);
}

#[test]
fn extend_on_unknown_schema_diagnoses() {
    let tree = analyze_fixture("schema_methods/extend_unknown.relon");
    let unknown = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::ExtendUnknownSchema { name, .. } if name == "Nonexistent"),
    );
    assert_eq!(unknown, 1, "{:?}", tree.diagnostics);
}

#[test]
fn duplicate_method_conflict() {
    let tree = analyze_fixture("schema_methods/method_conflict.relon");
    let conflicts = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MethodNameConflict { schema, method, .. }
            if schema == "Money" && method == "value")
    });
    assert_eq!(conflicts, 1, "{:?}", tree.diagnostics);
}

#[test]
fn cross_module_extend_visible_to_entry() {
    let ws = analyze_fixture_workspace("schema_methods_xmod", "entry.relon");
    let entry_id = ws
        .modules
        .keys()
        .find(|k| k.ends_with("entry.relon"))
        .cloned()
        .expect("entry module present");
    let entry_tree = ws.modules.get(&entry_id).expect("entry tree");
    let user_methods = entry_tree
        .schema_methods
        .get("User")
        .expect("User accumulated methods on entry");
    let names: Vec<_> = user_methods.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"greet"),
        "schema's own method propagated: {names:?}"
    );
    assert!(
        names.contains(&"is_admin"),
        "#extend method propagated: {names:?}"
    );
    let unknown = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::UnknownMethod { .. }))
        .count();
    assert_eq!(unknown, 0);
}

#[test]
fn self_calls_other_method_resolves() {
    let tree = analyze_fixture("schema_methods/self_calls_other_method.relon");
    let unknown = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownMethod { .. })
    });
    assert_eq!(unknown, 0, "{:?}", tree.diagnostics);
    let methods = tree.schema_methods.get("Money").unwrap();
    let names: Vec<_> = methods.iter().map(|m| m.name.as_str()).collect();
    assert!(names.contains(&"doubled"));
    assert!(names.contains(&"quadrupled"));
}

#[test]
fn extend_on_builtin_string() {
    let tree = analyze_fixture("schema_methods/extend_builtin.relon");
    let methods = tree.schema_methods.get("String").expect("String extended");
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "is_empty");
    // No `ExtendUnknownSchema` for built-in extension.
    let bad = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExtendUnknownSchema { .. })
    });
    assert_eq!(bad, 0, "{:?}", tree.diagnostics);
}

// ===================================================================
// Phase C.3: `#derive Constraint` witness shape checking.
// ===================================================================

#[test]
fn derive_equatable_with_matching_shape_no_diagnostic() {
    let tree = analyze_fixture("schema_methods/derive_equatable_ok.relon");
    let bad = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ConstraintWitnessShapeMismatch { .. })
    });
    assert_eq!(bad, 0, "{:?}", tree.diagnostics);
}

#[test]
fn derive_equatable_with_wrong_return_type_diagnoses() {
    let tree = analyze_fixture("schema_methods/derive_equatable_bad_shape.relon");
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::ConstraintWitnessShapeMismatch { constraint, method, .. }
                    if constraint == "Equatable" && method == "eq"
            )
        })
        .collect();
    assert_eq!(
        mismatches.len(),
        1,
        "expected ConstraintWitnessShapeMismatch on Equatable.eq: {:?}",
        tree.diagnostics
    );
}

// ===================================================================
// Phase C.4: auto-derive Equatable / JsonProjectable.
// ===================================================================

#[test]
fn auto_derive_synthesizes_eq_on_default_schema() {
    let tree = analyze_fixture("schema_methods/simple_method.relon");
    let methods = tree.schema_methods.get("Money").expect("Money present");
    // Auto-derived methods carry `is_native = true` + empty body.
    let synthesized_eq = methods
        .iter()
        .find(|m| m.name == "eq" && m.is_native && m.body_node.is_none());
    assert!(
        synthesized_eq.is_some(),
        "expected auto-derived eq on Money: {:?}",
        methods.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    let synthesized_to_json = methods
        .iter()
        .find(|m| m.name == "to_json" && m.is_native && m.body_node.is_none());
    assert!(
        synthesized_to_json.is_some(),
        "expected auto-derived to_json on Money: {:?}",
        methods.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

#[test]
fn no_auto_derive_equatable_skips_eq_synthesis() {
    let tree = analyze_fixture("schema_methods/no_auto_derive_equatable.relon");
    let methods = tree
        .schema_methods
        .get("Token")
        .cloned()
        .unwrap_or_default();
    let has_synthesized_eq = methods
        .iter()
        .any(|m| m.name == "eq" && m.is_native && m.body_node.is_none());
    assert!(
        !has_synthesized_eq,
        "Token opted out of Equatable but eq was synthesized: {:?}",
        methods.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    // JsonProjectable wasn't opted out, so to_json should still be
    // synthesized — sanity check that the opt-out is per-constraint.
    let has_to_json = methods
        .iter()
        .any(|m| m.name == "to_json" && m.is_native && m.body_node.is_none());
    assert!(
        has_to_json,
        "Token didn't opt out of JsonProjectable; to_json should be synthesized"
    );
}
