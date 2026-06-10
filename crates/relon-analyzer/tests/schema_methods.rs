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
    // Decision 21' (core.relon carrier): String comes pre-populated
    // with the built-in method set (`upper`, `lower`, `split`, ...).
    // The user-side `#extend String with { is_empty() ... }` adds one
    // method on top, so the total count is the carrier set + 1.
    let names: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
    assert!(
        names.contains(&"is_empty"),
        "user-side is_empty method present alongside core methods: {names:?}",
    );
    assert!(names.contains(&"upper"), "core method survives: {names:?}");
    // No `ExtendUnknownSchema` for built-in extension.
    let bad = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExtendUnknownSchema { .. })
    });
    assert_eq!(bad, 0, "{:?}", tree.diagnostics);
}

// ===================================================================
// Phase J: multi-hop receiver dispatch (`o.customer.greet()` etc.).
// `check_method_dispatch` and `resolve_call_signature` walk the path
// prefix via `infer::walk_path` so n>2 chains land on the same
// schema_methods table as the legacy 2-segment form.
// ===================================================================

#[test]
fn multi_hop_dispatch_resolves_method() {
    let tree = analyze_fixture("schema_methods/multi_hop_dispatch.relon");
    let errors: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnknownMethod { .. }))
        .collect();
    assert!(
        errors.is_empty(),
        "expected no UnknownMethod on multi-hop receiver: {:?}",
        tree.diagnostics
    );
    // The synthesized method signature lookup is keyed by `(Schema,
    // method)`. The multi-hop call still dispatches against
    // `(User, greet)`, regardless of how many fields preceded it.
    assert!(tree
        .method_signatures
        .contains_key(&("User".to_string(), "greet".to_string())));
}

#[test]
fn multi_hop_unknown_method_diagnoses() {
    let tree = analyze_fixture("schema_methods/multi_hop_unknown_method.relon");
    let unknown = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownMethod { schema, method, .. }
            if schema == "User" && method == "wave")
    });
    assert_eq!(
        unknown, 1,
        "expected UnknownMethod for User.wave: {:?}",
        tree.diagnostics
    );
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

#[test]
fn derive_unknown_constraint_name_diagnoses() {
    let tree = analyze_fixture("schema_methods/derive_unknown_constraint.relon");
    let unknown: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::UnknownDeriveConstraint { constraint, known, .. }
                    if constraint == "Comparble" && known.contains("Comparable")
            )
        })
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "expected UnknownDeriveConstraint on `Comparble`: {:?}",
        tree.diagnostics
    );
    // The misspelled pragma must not also fire a shape mismatch — the
    // unknown-name diagnostic owns the verdict.
    let shape = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ConstraintWitnessShapeMismatch { .. })
    });
    assert_eq!(shape, 0, "{:?}", tree.diagnostics);
}

#[test]
fn derive_known_constraint_names_stay_silent() {
    // Reverse guard for D2: every registered constraint name passes the
    // closed-set check (shape mismatches are a separate diagnostic).
    let tree = analyze_fixture("schema_methods/derive_equatable_ok.relon");
    let unknown = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownDeriveConstraint { .. })
    });
    assert_eq!(unknown, 0, "{:?}", tree.diagnostics);
}

// ===================================================================
// Phase C.6: shape checking for the four new constraint registry
// entries (Iterable / Indexable / Addable / Subtractable / ...) —
// witness shape is validated even though their operator lowering
// (for / a[i] / arithmetic) is still TODO.
// ===================================================================

#[test]
fn derive_addable_with_matching_shape_no_diagnostic() {
    let tree = analyze_fixture("schema_methods/derive_addable_ok.relon");
    let bad = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ConstraintWitnessShapeMismatch { .. })
    });
    assert_eq!(bad, 0, "{:?}", tree.diagnostics);
}

#[test]
fn derive_addable_with_wrong_shape_diagnoses() {
    let tree = analyze_fixture("schema_methods/derive_addable_bad_shape.relon");
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::ConstraintWitnessShapeMismatch { constraint, method, .. }
                    if constraint == "Addable" && method == "add"
            )
        })
        .collect();
    assert_eq!(
        mismatches.len(),
        1,
        "expected ConstraintWitnessShapeMismatch on Addable.add: {:?}",
        tree.diagnostics
    );
}

#[test]
fn derive_indexable_with_matching_shape_populates_method_table() {
    // Decision 22 (Indexable lowering): the source-level `#derive
    // Indexable` + `index(key: K) -> Option<V>` witness should pass
    // the shape check and land in `schema_methods["Bag"]` so the
    // evaluator can dispatch `bag[i]` against it.
    let tree = analyze_fixture("schema_methods/index_dispatch.relon");
    let bad = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ConstraintWitnessShapeMismatch { .. })
    });
    assert_eq!(bad, 0, "{:?}", tree.diagnostics);
    let methods = tree
        .schema_methods
        .get("Bag")
        .expect("Bag should have a method table entry");
    let index_method = methods
        .iter()
        .find(|m| m.name == "index")
        .expect("Bag.index should be registered");
    assert!(index_method.body_node.is_some(), "Bag.index expects a body");
    assert_eq!(
        index_method.params.len(),
        1,
        "Bag.index expects one param (`key`)"
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

// ===================================================================
// Method-level generics (parser-supported as of the follow-up to
// schema-rooted-model-2026-05-11 decision 4): `map<U>(...)` etc.
// flow from parser SchemaMethod.generics into analyzer
// SchemaMethodInfo.generics into the synthesized FnSignature.generics
// so the existing `sig::instantiate` machinery binds them at call
// sites. Carrier `core/list.relon` ships `map<U>` / `reduce<U>` as
// the canonical examples.
// ===================================================================

#[test]
fn core_list_map_carries_method_level_generics() {
    // Empty source — the core carriers inject themselves regardless.
    let tree = relon_analyzer::analyze(
        &relon_parser::parse_document("{}").expect("empty document parses"),
    );
    let methods = tree
        .schema_methods
        .get("List")
        .expect("core carrier installs List schema");
    let map = methods
        .iter()
        .find(|m| m.name == "map")
        .expect("List.map present");
    assert_eq!(
        map.generics,
        vec!["U".to_string()],
        "List.map declares the U placeholder: {:?}",
        map.generics
    );
    let reduce = methods
        .iter()
        .find(|m| m.name == "reduce")
        .expect("List.reduce present");
    assert_eq!(reduce.generics, vec!["U".to_string()]);
    // Monomorphic methods stay empty.
    let len_ = methods
        .iter()
        .find(|m| m.name == "len")
        .expect("List.len present");
    assert!(len_.generics.is_empty(), "len has no method-level generics");
}

#[test]
fn method_signature_table_propagates_method_generics() {
    let tree = relon_analyzer::analyze(
        &relon_parser::parse_document("{}").expect("empty document parses"),
    );
    let sig = tree
        .method_signatures
        .get(&("List".to_string(), "map".to_string()))
        .expect("List.map signature synthesized");
    assert_eq!(
        sig.generics,
        vec!["U".to_string()],
        "synthesized signature carries method-level generics: {:?}",
        sig.generics
    );
    // Schema-level T must *not* be re-declared on the method signature
    // — it's bound by the receiver's concrete type at dispatch time.
    assert!(
        !sig.generics.iter().any(|g| g == "T"),
        "schema-level T should not appear in method signature.generics: {:?}",
        sig.generics
    );
}

// ===================================================================
// Schema-rooted §J follow-up: `MethodGenericShadowsSchemaGeneric`
// surfaces when a method's `<T>` collides with one of the owning
// schema's `<T>` parameters. Substitution treats the two names as a
// single binding key — the warning lets authors see the smell before
// debugging mysterious instantiation results.
// ===================================================================

#[test]
fn method_generic_shadowing_emits_warning() {
    let tree = analyze_fixture("schema_methods/generic_shadow_warn.relon");
    let warnings: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(d,
                Diagnostic::MethodGenericShadowsSchemaGeneric { schema, method, generic, .. }
                    if schema == "Box" && method == "pick" && generic == "T"
            )
        })
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "expected one shadow warning on Box::pick<T>: {:?}",
        tree.diagnostics
    );
    // Severity is Warning — the program is still well-formed.
    assert_eq!(
        warnings[0].severity(),
        relon_analyzer::Severity::Warning,
        "shadow diagnostic should be Warning, not Error"
    );
}

#[test]
fn method_generic_with_distinct_name_stays_silent() {
    let tree = analyze_fixture("schema_methods/generic_shadow_distinct.relon");
    let warnings = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MethodGenericShadowsSchemaGeneric { .. })
    });
    assert_eq!(
        warnings, 0,
        "distinct generic names must not trigger the shadow warning: {:?}",
        tree.diagnostics
    );
}

// ===================================================================
// Schema-rooted §J follow-up: `MethodGenericArgMismatch` flags an
// `index(key: K)` call site whose actual key type contradicts the
// constraint-substituted `K`. Polymorphic witnesses stay silent.
// ===================================================================

#[test]
fn index_key_mismatch_emits_diagnostic() {
    let tree = analyze_fixture("schema_methods/index_dispatch_key_mismatch.relon");
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(d,
                Diagnostic::MethodGenericArgMismatch { schema, method, expected, found, .. }
                    if schema == "Bag" && method == "index" && expected == "Int" && found == "String"
            )
        })
        .collect();
    assert_eq!(
        mismatches.len(),
        1,
        "expected one MethodGenericArgMismatch on Bag.index: {:?}",
        tree.diagnostics
    );
}

#[test]
fn index_key_ok_stays_silent() {
    let tree = analyze_fixture("schema_methods/index_dispatch_key_ok.relon");
    let mismatches = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MethodGenericArgMismatch { .. })
    });
    assert_eq!(
        mismatches, 0,
        "matching key type must not trigger MethodGenericArgMismatch: {:?}",
        tree.diagnostics
    );
}

#[test]
fn index_key_polymorphic_witness_stays_silent() {
    let tree = analyze_fixture("schema_methods/index_dispatch_key_polymorphic.relon");
    let mismatches = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MethodGenericArgMismatch { .. })
    });
    assert_eq!(
        mismatches, 0,
        "polymorphic K must not surface MethodGenericArgMismatch: {:?}",
        tree.diagnostics
    );
}

#[test]
fn monomorphic_method_on_generic_schema_stays_silent() {
    // `#schema Box<T>` with a method that has no generics of its own
    // (e.g. `eq(other: Self) -> Bool`) is the regression-bait case
    // called out in the task description. Must not warn.
    let src = r#"
#schema Box<T> with {
    eq(other: Self) -> Bool: true
}
{}
"#;
    let node = relon_parser::parse_document(src).expect("parse");
    let tree = relon_analyzer::analyze(&node);
    let warnings = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MethodGenericShadowsSchemaGeneric { .. })
    });
    assert_eq!(
        warnings, 0,
        "method with no generics of its own should never warn: {:?}",
        tree.diagnostics
    );
}
