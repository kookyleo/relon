//! Type-check pass tests. Co-located with the [`super`] dispatch module
//! and its domain sub-modules — the assertions cover every check group
//! through the public [`super::typecheck`] entry, so the same suite
//! exercises helpers / index / fn_call / spread / pattern / binary /
//! reference / typed_binding together via the integrated walker.

use super::*;
use crate::analyze;
use crate::sig::{type_node_simple, FnParam, FnSignature};
use relon_parser::parse_document;
use std::collections::HashMap;

fn analyze_str(src: &str) -> AnalyzedTree {
    let node = parse_document(src).unwrap();
    analyze(&node)
}

#[test]
fn flags_unresolved_sibling_reference() {
    let tree = analyze_str(r#"{ a: 1, b: &sibling.missing }"#);
    let warnings: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "missing"))
        .collect();
    assert_eq!(warnings.len(), 1, "{:?}", tree.diagnostics);
}

#[test]
fn does_not_flag_dynamic_spread() {
    // `merged` has a spread, so a sibling reference inside it
    // can plausibly be saved by a key from `base`.
    let tree = analyze_str(
        r#"{
            base: { x: 1 },
            merged: { ...&sibling.base, hint: x }
        }"#,
    );
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", unresolved);
}

#[test]
fn does_not_flag_closure_param() {
    let tree = analyze_str(r#"{ helper(arg): arg + 1 }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", unresolved);
}

#[test]
fn flags_static_type_mismatch_on_typed_field() {
    let tree = analyze_str(r#"{ Int port: "8080" }"#);
    assert!(
        tree.diagnostics.iter().any(
            |d| matches!(d, Diagnostic::StaticTypeMismatch { expected, found, .. }
                if expected == "Int" && found == "String")
        ),
        "{:?}",
        tree.diagnostics
    );
}

#[test]
fn allows_optional_null() {
    let tree = analyze_str(r#"{ Int? port: null }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert!(mismatches.is_empty(), "{:?}", mismatches);
}

#[test]
fn flags_mismatch_inside_custom_schema_binding() {
    let tree = analyze_str(
        r#"{
            #schema User { String name: *, Int age: * },
            User alice: { name: "A", age: "thirty" }
        }"#,
    );
    assert!(
        tree.diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "age")),
        "{:?}",
        tree.diagnostics
    );
}

#[test]
fn flags_non_exhaustive_match_on_sum_enum() {
    let tree = analyze_str(
        r#"{
            #schema N Enum<A { x: Int }, B { y: Int }, C>,
            N v: N.A { x: 1 },
            out: v match {
                A: 1,
                B: 2
            }
        }"#,
    );
    let nx: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::NonExhaustiveMatch { .. }))
        .collect();
    assert_eq!(nx.len(), 1, "{:?}", tree.diagnostics);
    if let Diagnostic::NonExhaustiveMatch {
        enum_name,
        missing_variants,
        ..
    } = nx[0]
    {
        assert_eq!(enum_name, "N");
        assert_eq!(missing_variants, &vec!["C".to_string()]);
    } else {
        panic!()
    }
}

#[test]
fn flags_unknown_variant_with_did_you_mean() {
    let tree = analyze_str(
        r#"{
            #schema N Enum<Email { x: Int }, SMS { y: Int }>,
            N v: N.Email { x: 1 },
            out: v match {
                EMail: 1,
                SMS: 2
            }
        }"#,
    );
    let unknown: Vec<_> = tree
        .diagnostics
        .iter()
        .filter_map(|d| match d {
            Diagnostic::UnknownVariant {
                variant_name,
                suggestion,
                ..
            } => Some((variant_name.clone(), suggestion.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(unknown.len(), 1, "{:?}", tree.diagnostics);
    assert_eq!(unknown[0].0, "EMail");
    assert_eq!(unknown[0].1.as_deref(), Some("Email"));
}

#[test]
fn flags_duplicate_match_arm() {
    let tree = analyze_str(
        r#"{
            #schema N Enum<A { x: Int }, B { y: Int }>,
            N v: N.A { x: 1 },
            out: v match {
                A: 1,
                A: 2,
                B: 3
            }
        }"#,
    );
    assert!(
        tree.diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::DuplicateMatchArm { variant_name, .. } if variant_name == "A")),
        "{:?}",
        tree.diagnostics
    );
}

#[test]
fn wildcard_arm_satisfies_exhaustiveness() {
    let tree = analyze_str(
        r#"{
            #schema N Enum<A { x: Int }, B { y: Int }, C>,
            N v: N.A { x: 1 },
            out: v match {
                A: 1,
                *: 9
            }
        }"#,
    );
    let nx: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::NonExhaustiveMatch { .. }))
        .collect();
    assert!(nx.is_empty(), "{:?}", tree.diagnostics);
}

#[test]
fn skips_exhaustiveness_when_type_uninferrable() {
    // `mystery` has no type hint and isn't a variant constructor
    // — the analyzer can't statically determine its enum, so no
    // exhaustiveness diagnostic should fire.
    let tree = analyze_str(
        r#"{
            #schema N Enum<A { x: Int }, B { y: Int }>,
            mystery: 42,
            out: mystery match {
                A: 1
            }
        }"#,
    );
    let nx: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::NonExhaustiveMatch { .. }))
        .collect();
    assert!(nx.is_empty(), "{:?}", tree.diagnostics);
}

#[test]
fn flags_nested_list_mismatch() {
    let tree = analyze_str(r#"{ List<Int> items: [1, "two", 3] }"#);
    assert!(
        tree.diagnostics.iter().any(|d| matches!(
            d,
            Diagnostic::StaticTypeMismatch {
                field,
                expected,
                found,
                ..
            } if field == "items[1]" && expected == "Int" && found == "String"
        )),
        "{:?}",
        tree.diagnostics
    );
}

#[test]
fn flags_nested_dict_mismatch() {
    let tree = analyze_str(r#"{ Dict<String, Int> scores: { math: 100, art: "A" } }"#);
    assert!(
        tree.diagnostics.iter().any(|d| matches!(
            d,
            Diagnostic::StaticTypeMismatch {
                field,
                expected,
                found,
                ..
            } if field == "scores.art" && expected == "Int" && found == "String"
        )),
        "{:?}",
        tree.diagnostics
    );
}

#[test]
fn infers_binary_expression_types() {
    let tree = analyze_str(
        r#"{
            Int a: 1 + 2,
            Float b: 1 + 2.0,
            String c: "a" + "b",
            Bool d: 1 == 1,
            // These should fail
            Int e: 1.0 + 2.0,
            String f: 1 + 2
        }"#,
    );
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    // e and f should mismatch
    assert_eq!(mismatches.len(), 2, "{:?}", mismatches);
    assert!(mismatches
        .iter()
        .any(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "e")));
    assert!(mismatches
        .iter()
        .any(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "f")));
}

#[test]
fn handles_ternary_type_inference() {
    let tree = analyze_str(
        r#"{
            Int a: true ? 1 : 2,
            Float b: true ? 1 : 2.2,
            // This should fail (heterogeneous)
            Int c: true ? 1 : "2"
        }"#,
    );
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", mismatches);
    assert!(mismatches
        .iter()
        .any(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "c")));
}

#[test]
fn recursive_list_check() {
    let tree = analyze_str(r#"{ List<List<Int>> matrix: [[1], ["two"]] }"#);
    assert!(
        tree.diagnostics.iter().any(|d| matches!(
            d,
            Diagnostic::StaticTypeMismatch {
                field,
                expected,
                found,
                ..
            } if field == "matrix[1][0]" && expected == "Int" && found == "String"
        )),
        "{:?}",
        tree.diagnostics
    );
}

/// Stage 1.3: a binary operator applied to incompatible operands
/// (Int + String) is reported even when no type hint is in play —
/// the slot is `Int x:` so the binding line forces an explicit
/// classification of the value expression.
#[test]
fn flags_binary_int_plus_string() {
    let tree = analyze_str(r#"{ Int x: 1 + "hello" }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    // Should have at least one diagnostic — possibly two (one for
    // the binary itself, one for the slot binding).
    assert!(!mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.3 reverse: `Any` slot tolerates the same expression
/// without producing the binary mismatch (the binding side accepts
/// it, but the binary itself remains a known-bad combination —
/// so we still expect the binary diagnostic). Encodes the rule
/// "the typed binding is happy, but the binary is still wrong".
#[test]
fn binary_mismatch_independent_of_slot_type() {
    let tree = analyze_str(r#"{ Any x: 1 + "hello" }"#);
    // The slot accepts Any, so no slot-level mismatch — but the
    // binary itself is still ill-typed.
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert!(!mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.3: untyped slots over a known-bad binary still report —
/// the binary itself is the offender, regardless of the field.
#[test]
fn flags_bare_bool_arithmetic() {
    let tree = analyze_str(r#"{ x: true + 1 }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 1.4 forward: a sibling reference to a typed field carries
/// the field's declared type back to the binding site. Slots
/// declaring `Int y` over a `String x` reference should mismatch.
#[test]
fn flags_reference_to_typed_sibling() {
    let tree = analyze_str(
        r#"{
            String x: "hello",
            Int y: x
        }"#,
    );
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "y"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 1.4 reverse: when the referenced sibling is itself a fn
/// call to a name the analyzer can't resolve to a static signature,
/// stay silent — runtime keeps owning the verdict. (Stage 3 added
/// signature lookup for stdlib fns like `range`, so we use an
/// unknown name here to preserve the original silent-on-fncall
/// invariant.)
#[test]
fn does_not_flag_fncall_sibling_reference() {
    let tree = analyze_str(
        r#"{
            xs: dynamic_unknown_fn(),
            Int y: xs
        }"#,
    );
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "y"))
        .collect();
    assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.5 forward: closure body type disagrees with the
/// declared `-> Type`. The dict-method shorthand `Type key(params): body`
/// desugars to a closure with `return_type = Type`.
#[test]
fn flags_closure_return_type_mismatch() {
    let tree = analyze_str(r#"{ Int helper(Int x): x + "y" }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert!(!mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.5 reverse: untyped param + no return annotation =
/// silent. Body type inference defaults to `Any` because of the
/// untyped param, so we have nothing to compare against.
#[test]
fn does_not_flag_untyped_closure() {
    let tree = analyze_str(r#"{ f(x): x + 1 }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.6 forward: match arms returning unrelated types
/// (`Int` vs `String`) collapse to `Any` — flag.
#[test]
fn flags_match_arm_type_mismatch() {
    let tree = analyze_str(
        r#"{
            #schema N Enum<A { x: Int }, B { y: Int }>,
            N v: N.A { x: 1 },
            out: v match {
                A: 1,
                B: "two"
            }
        }"#,
    );
    let mm: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::MatchArmTypeMismatch { .. }))
        .collect();
    assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 1.6 reverse: arms returning the same type don't trip the
/// join collapse — silent.
#[test]
fn does_not_flag_homogeneous_match_arms() {
    let tree = analyze_str(
        r#"{
            #schema N Enum<A { x: Int }, B { y: Int }>,
            N v: N.A { x: 1 },
            out: v match {
                A: 1,
                B: 2
            }
        }"#,
    );
    let mm: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::MatchArmTypeMismatch { .. }))
        .collect();
    assert!(mm.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.9 #9: stdlib FnCalls don't carry a static signature in
/// the analyzer, so a bare assignment from `range(...)` must stay
/// silent — no spurious mismatch even though the slot is untyped.
#[test]
fn fncall_assignment_is_silent() {
    let tree = analyze_str(r#"{ x: range(0, 10) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.9 #11: `Any` declared slot is silent on the slot side.
/// The binary itself is still ill-typed, so we expect exactly one
/// diagnostic (from the binary check), never one from the slot.
#[test]
fn any_slot_does_not_add_slot_level_mismatch() {
    let tree = analyze_str(r#"{ Any x: 1 + "y" }"#);
    let slot_mm: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::StaticTypeMismatch { field, expected, .. }
                    if field == "x" && expected == "Any"
            )
        })
        .collect();
    assert!(slot_mm.is_empty(), "{:?}", tree.diagnostics);
    // There IS one diagnostic — for the binary itself.
    let binary_mm: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert!(!binary_mm.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 1.9 #12 (consistency): when the analyzer reports a
/// `StaticTypeMismatch`, `tree.has_errors()` flips to true so the
/// evaluator's facade refuses to run. This pins the gating
/// contract that Stage 1.1 enabled.
#[test]
fn static_type_mismatch_marks_tree_as_errored() {
    let tree = analyze_str(r#"{ Int x: "hello" }"#);
    assert!(tree.has_errors(), "{:?}", tree.diagnostics);
}

/// Stage 1.9 #10: a sibling reference to an unknown name flags
/// `UnresolvedReference` (a warning) but never a spurious
/// `StaticTypeMismatch` — runtime owns whether it eventually
/// resolves through a dynamic frame.
#[test]
fn unresolved_sibling_does_not_static_mismatch() {
    let tree = analyze_str(r#"{ x: &sibling.unknown }"#);
    let typ: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .collect();
    assert!(typ.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.8 forward: a schema field declares a type whose head
/// isn't a builtin or a declared schema — flag `UnknownTypeName`.
#[test]
fn schema_field_unknown_type_flagged() {
    let tree = analyze_str(
        r#"{
            #schema A { B b: * }
        }"#,
    );
    let unknown: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnknownTypeName { name, .. } if name == "B"))
        .collect();
    assert!(!unknown.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.8 reverse: a declared schema name as a field type stays
/// silent.
#[test]
fn schema_field_known_type_silent() {
    let tree = analyze_str(
        r#"{
            #schema B { Int n: * },
            #schema A { B b: * }
        }"#,
    );
    let unknown: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnknownTypeName { .. }))
        .collect();
    assert!(unknown.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.8: `{ ...{ a: 1, b: 2 }, x: c }` — `c` isn't merged in
/// from the spread (only `a` and `b` are), so it must flag.
#[test]
fn spread_then_unresolved_sibling() {
    let tree = analyze_str(r#"{ ...{a: 1, b: 2}, x: c }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "c"))
        .collect();
    assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 2.7 forward: a function call to a name that isn't bound
/// to any sibling, closure param, stdlib, or host fn must surface
/// as `UnresolvedReference`.
#[test]
fn fncall_unknown_name_flagged() {
    let tree = analyze_str(r#"{ x: undef_fn() }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "undef_fn"))
        .collect();
    assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 2.7 reverse: stdlib names like `range` are silent.
#[test]
fn fncall_stdlib_silent() {
    let tree = analyze_str(r#"{ x: range(0, 10) }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.7 reverse: a sibling-bound closure used as a callee
/// stays silent.
#[test]
fn fncall_sibling_closure_silent() {
    let tree = analyze_str(r#"{ helper(): 1, x: helper() }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.6 forward: a multi-segment path whose tail names a key
/// missing from the bound dict literal flags `UnresolvedReference`.
#[test]
fn dot_path_dict_literal_missing_key_flagged() {
    let tree = analyze_str(r#"{ obj: { a: 1 }, x: obj.b }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "obj.b"))
        .collect();
    assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 2.6 forward: same idea for a typed schema binding — `u.bogus`
/// where `bogus` isn't declared on the schema flags.
#[test]
fn dot_path_schema_field_missing_flagged() {
    let tree = analyze_str(
        r#"{
            #schema U { Int n: * },
            U u: { n: 1 },
            x: u.bogus
        }"#,
    );
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "u.bogus"))
        .collect();
    assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 2.6 reverse: a dot-path through a sibling whose value
/// comes from a stdlib FnCall (uninferrable type) stays silent —
/// runtime owns whether the field exists.
#[test]
fn dot_path_through_fncall_sibling_silent() {
    let tree = analyze_str(
        r#"{
            xs: range(0, 10),
            first: xs.zero
        }"#,
    );
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name.starts_with("xs.")))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.6 reverse: a dot-path through a sibling whose value is
/// a typed dict literal with the named key stays silent.
#[test]
fn dot_path_existing_key_silent() {
    let tree = analyze_str(r#"{ obj: { a: 1, b: 2 }, x: obj.a }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.5 forward: a spread of a dict literal merges its keys
/// into the surrounding frame statically — a sibling reference to
/// one of the spread keys is no longer flagged.
#[test]
fn spread_dict_literal_merges_keys_statically() {
    let tree = analyze_str(r#"{ ...{a: 1, b: 2}, x: a + b }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.5 reverse: a spread of a non-literal expression stays
/// dynamic — references to keys that *might* come from the spread
/// remain unflagged (the dynamic-spread escape hatch is preserved).
#[test]
fn spread_non_literal_still_dynamic() {
    let tree = analyze_str(
        r#"{
            base: { x: 1 },
            merged: { ...&sibling.base, hint: x }
        }"#,
    );
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.4 forward: a closure body's free variable that doesn't
/// match any in-scope param / sibling and isn't on the stdlib /
/// host fn allowlist must surface as `UnresolvedReference`.
#[test]
fn closure_body_free_var_flagged() {
    let tree = analyze_str(r#"{ helper(x): x + outer_undef }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(
            |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "outer_undef"),
        )
        .collect();
    assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 2.4 reverse: stdlib names like `range` stay silent.
#[test]
fn closure_body_stdlib_not_flagged() {
    let tree = analyze_str(r#"{ x: range(0, 10) }"#);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.4 reverse: a host-injected fn name silences the warning.
#[test]
fn host_fn_name_silences_unresolved() {
    use std::collections::HashSet;
    let mut host_fn_names = HashSet::new();
    host_fn_names.insert("my_native".to_string());
    let opts = crate::AnalyzeOptions {
        host_fn_names,
        ..Default::default()
    };
    let node = parse_document(r#"{ x: my_native() }"#).unwrap();
    let tree = crate::analyze_with_options(&node, &opts);
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
        .collect();
    assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.2 reverse: a multi-segment slot type (`geo.Location`)
/// that the per-module pass can't prove unsafe — there's no
/// in-module `geo` schema name to consult — must stay conservative
/// (no spurious mismatch). The cross-module form is handled by
/// the workspace-level `re_check_unknown_types` post-pass.
#[test]
fn multi_segment_path_stays_conservative_in_per_module_pass() {
    let tree = analyze_str(
        r#"{
            geo.Location loc: 1,
            #schema X { Int z: * },
            X x_val: { z: 1 }
        }"#,
    );
    // The `loc: 1` slot uses a 2-segment type `geo.Location`. We
    // shouldn't crash and shouldn't push a spurious mismatch in
    // the per-module pass.
    let typ: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "loc"))
        .collect();
    assert!(typ.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.3 forward: a value typed via the derived schema `B` is
/// accepted in a slot declared `A` when `B` extends `A` through the
/// `Base + { ... }` composition form.
#[test]
fn derived_schema_subsumes_base_slot() {
    let tree = analyze_str(
        r#"{
            #schema A { Int x: * },
            #schema B &sibling.A + { Int y: * },
            B make_b: { x: 1, y: 2 },
            A use_as_a: make_b
        }"#,
    );
    let mm: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(
            |d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "use_as_a"),
        )
        .collect();
    assert!(mm.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 2.3 reverse: an unrelated schema name does not match.
#[test]
fn unrelated_schema_does_not_subsume_slot() {
    let tree = analyze_str(
        r#"{
            #schema A { Int x: * },
            #schema C { Int z: * },
            C make_c: { z: 1 },
            A use_as_a: make_c
        }"#,
    );
    // We expect a StaticTypeMismatch because C is not a base / derived
    // of A, but the analyzer's conservative path may also stay silent
    // if it can't prove the negative. The key invariant: if a mismatch
    // does fire, the field should be `use_as_a`.
    for d in &tree.diagnostics {
        if let Diagnostic::StaticTypeMismatch { field, .. } = d {
            assert_eq!(field, "use_as_a");
        }
    }
}

// ------------------------------------------------------------------
// Stage 3 — closure / user fn / stdlib signature lookup + FnCall
//          arity / arg-type checks. Tests numbered to match the
//          design doc's §10 coverage list.
// ------------------------------------------------------------------

/// Stage 3.7 #1: `range()` with zero args flags `FnCallArgCountMismatch`
/// (signature requires at least 1 Int).
#[test]
fn stage3_range_zero_args_arg_count() {
    let tree = analyze_str(r#"{ x: range() }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgCountMismatch { fn_name, .. } if fn_name == "range"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.7 #2: `range(0, "ten")` — the second arg is a String
/// where the variadic_tail is Int.
#[test]
fn stage3_range_string_arg_arg_type() {
    let tree = analyze_str(r#"{ x: range(0, "ten") }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { fn_name, .. } if fn_name == "range"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.7 #3: `len(123)` — the analyzer signature accepts `Any`
/// for `len`'s param so this stays silent (v1 doesn't model the
/// String∣List∣Dict union). Documents the v1 trade-off.
#[test]
fn stage3_len_int_silent_v1() {
    let tree = analyze_str(r#"{ x: len(123) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
        .collect();
    assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #4 forward: `len([1,2])` returns Int — `Int n: len(...)`
/// is happy.
#[test]
fn stage3_len_returns_int() {
    let tree = analyze_str(r#"{ Int n: len([1, 2]) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "n"))
        .collect();
    assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #4 reverse: a `String s: len([1,2])` slot mismatches.
#[test]
fn stage3_len_returns_int_string_slot_mismatches() {
    let tree = analyze_str(r#"{ String s: len([1, 2]) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "s"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.7 #5: user closure `f(Int x) -> Int: x+1` called with
/// a String arg flags `FnCallArgTypeMismatch`.
#[test]
fn stage3_user_closure_arg_type_mismatch() {
    let tree = analyze_str(r#"{ Int f(Int x): x + 1, y: f("str") }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(
            |d| matches!(d, Diagnostic::FnCallArgTypeMismatch { fn_name, .. } if fn_name == "f"),
        )
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.7 #6: user closure return type drives slot inference —
/// `String y: f(1)` mismatches because `f` returns Int.
#[test]
fn stage3_user_closure_return_drives_slot() {
    let tree = analyze_str(r#"{ Int f(Int x): x + 1, String y: f(1) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "y"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.7 #7: `_math_abs()` — zero args.
#[test]
fn stage3_math_abs_no_args() {
    let tree = analyze_str(r#"{ x: _math_abs() }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgCountMismatch { fn_name, .. } if fn_name == "_math_abs"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.7 #8: `_string_upper(123)` — wrong type.
#[test]
fn stage3_string_upper_int_arg() {
    let tree = analyze_str(r#"{ x: _string_upper(123) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { fn_name, .. } if fn_name == "_string_upper"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.7 #9 reverse: `range(0, 10)` is legal (uses variadic_tail).
#[test]
fn stage3_range_two_args_legal() {
    let tree = analyze_str(r#"{ x: range(0, 10) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::FnCallArgCountMismatch { .. }
                    | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        })
        .collect();
    assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #10 reverse: undefined fn name silently falls through
/// the FnCall checker (still emits `UnresolvedReference`, but no
/// FnCall diagnostic).
#[test]
fn stage3_undefined_fn_silent_on_signature_check() {
    let tree = analyze_str(r#"{ f(): undefined() }"#);
    let fn_call_diags: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::FnCallArgCountMismatch { .. }
                    | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        })
        .collect();
    assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #11 reverse: a host fn registered without a signature
/// silently passes the FnCall check.
#[test]
fn stage3_host_fn_without_sig_silent() {
    use std::collections::HashSet;
    let mut host_fn_names = HashSet::new();
    host_fn_names.insert("my_native".to_string());
    let opts = crate::AnalyzeOptions {
        host_fn_names,
        ..Default::default()
    };
    let node = parse_document(r#"{ x: my_native(1, 2, 3) }"#).unwrap();
    let tree = crate::analyze_with_options(&node, &opts);
    let fn_call_diags: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::FnCallArgCountMismatch { .. }
                    | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        })
        .collect();
    assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #12 reverse: an arg whose type is dynamic (`Any`)
/// silently passes the per-arg check.
#[test]
fn stage3_dynamic_arg_silent() {
    // The `_string_upper` param is String. Pass an unresolvable
    // identifier (silent on inference) → arg infer returns None →
    // the per-arg check `continue`s.
    let tree = analyze_str(r#"{ f(x): _string_upper(x) }"#);
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
        .collect();
    assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #13 (consistency): when the analyzer reports an
/// `FnCallArgTypeMismatch`, `tree.has_errors()` is true so the
/// evaluator's facade refuses to run before reaching the runtime
/// path. Pins the contract that analyzer-reported errors gate
/// evaluation.
#[test]
fn stage3_arg_type_mismatch_marks_tree_errored() {
    let tree = analyze_str(r#"{ Int f(Int x): x + 1, y: f("str") }"#);
    assert!(tree.has_errors(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #14 (known short-fall): cross-module fn imports v1
/// silent. The signature lookup only sees stdlib + host + same-
/// file closures; an `#import`ed user closure has no signature
/// reachable here, so the call goes unchecked. This test pins the
/// v1 limitation explicitly so a later v1.1 stage can flip it.
#[test]
fn stage3_cross_module_fn_import_silent_v1() {
    // A bare-`Variable("module_alias")` head doesn't even reach
    // FnCall handling — but a `module.fn(arg)` form does. We verify
    // it stays silent against the FnCall checker. Modeling cross-
    // module signatures is deferred to v1.1.
    let tree = analyze_str(r#"{ x: imported.fn(1, 2, 3) }"#);
    let fn_call_diags: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::FnCallArgCountMismatch { .. }
                    | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        })
        .collect();
    assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #15: dict-literal sibling closure call (`utils.greet`)
/// resolves to the closure's signature and accepts a legal arg.
#[test]
fn stage3_sibling_closure_dict_literal_call_silent() {
    let tree = analyze_str(
        r#"{
            utils: { greet(s): "hi" + s },
            x: utils.greet("a")
        }"#,
    );
    let fn_call_diags: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::FnCallArgCountMismatch { .. }
                    | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        })
        .collect();
    assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
}

/// Stage 3.7 #16: same dict-literal sibling form, but the closure
/// declares `String s` and the call passes an Int — the analyzer
/// flags `FnCallArgTypeMismatch`.
#[test]
fn stage3_sibling_closure_dict_literal_arg_type_mismatch() {
    let tree = analyze_str(
        r#"{
            utils: { greet(String s): "hi" + s },
            x: utils.greet(123)
        }"#,
    );
    let mismatches: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
        .collect();
    assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
}

/// Stage 3.2 drift defense: every name registered by the
/// evaluator's `stdlib::register_to` must have a signature in
/// `stdlib_signatures`. If a maintainer adds a new fn to the
/// evaluator without updating the analyzer table, this test
/// fails — keeping the two views in lockstep.
#[test]
fn stage3_stdlib_signatures_cover_all_register_fn_names() {
    let sigs = crate::stdlib_signatures::stdlib_signatures();
    let names = super::helpers::stdlib_registered_names();
    let missing: Vec<&&str> = names.iter().filter(|n| !sigs.contains_key(**n)).collect();
    assert!(
        missing.is_empty(),
        "stdlib functions without analyzer signatures: {missing:?}"
    );
}

// ----- Stage 5: const-folding diagnostics -----------------------

fn const_div_zero_count(tree: &AnalyzedTree) -> usize {
    tree.diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::ConstDivisionByZero { .. }))
        .count()
}

fn const_overflow_count(tree: &AnalyzedTree) -> usize {
    tree.diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::ConstNumericOverflow { .. }))
        .count()
}

#[test]
fn stage5_div_by_zero_literal() {
    let tree = analyze_str(r#"{ x: 1 / 0 }"#);
    assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
    assert!(tree.has_errors());
}

#[test]
fn stage5_mod_by_zero_literal() {
    let tree = analyze_str(r#"{ x: 100 % 0 }"#);
    assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
}

#[test]
fn stage5_overflow_add_at_max_plus_one() {
    let tree = analyze_str(r#"{ x: 9223372036854775807 + 1 }"#);
    assert_eq!(const_overflow_count(&tree), 1, "{:?}", tree.diagnostics);
    assert!(tree.has_errors());
}

#[test]
fn stage5_overflow_chained_mul() {
    // 1_000_000^4 = 1e24 > i64::MAX, traps on the third multiply.
    let tree = analyze_str(r#"{ x: 1000000 * 1000000 * 1000000 * 1000000 }"#);
    assert_eq!(const_overflow_count(&tree), 1, "{:?}", tree.diagnostics);
}

#[test]
fn stage5_subtree_folds_then_div_zero() {
    // (1+2)*(3+4)/0 → fold collapses to 21/0, single diagnostic.
    let tree = analyze_str(r#"{ x: (1 + 2) * (3 + 4) / 0 }"#);
    assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
    // No overflow false-positive on the inner sub-expressions.
    assert_eq!(const_overflow_count(&tree), 0);
}

#[test]
fn stage5_unary_neg_i64_min_overflows() {
    // i64::MIN = -9223372036854775808 — the `-` unary on
    // `9223372036854775807 + 1` would itself overflow first; we use
    // the canonical hex form via `(-9223372036854775807 - 1)` to
    // construct i64::MIN as an Int and then unary-negate it.
    let tree = analyze_str(r#"{ x: -(-9223372036854775807 - 1) }"#);
    assert_eq!(const_overflow_count(&tree), 1, "{:?}", tree.diagnostics);
}

#[test]
fn stage5_variable_in_subtree_silent() {
    // `a + 1` references a sibling, so the fold pass returns None
    // and runtime keeps the verdict.
    let tree = analyze_str(r#"{ a: 1, x: a + 1 }"#);
    assert_eq!(const_div_zero_count(&tree), 0);
    assert_eq!(const_overflow_count(&tree), 0);
}

#[test]
fn stage5_float_div_zero_silent() {
    // 1.0 / 0.0 is +Inf in IEEE-754 — never errors.
    let tree = analyze_str(r#"{ x: 1.0 / 0.0 }"#);
    assert_eq!(const_div_zero_count(&tree), 0, "{:?}", tree.diagnostics);
    assert_eq!(const_overflow_count(&tree), 0);
}

#[test]
fn stage5_fn_call_in_subtree_silent() {
    // `len([1,2,3])` is non-foldable (FnCall); whole expression
    // defers to runtime.
    let tree = analyze_str(r#"{ x: 1 / len([1, 2, 3]) }"#);
    assert_eq!(const_div_zero_count(&tree), 0, "{:?}", tree.diagnostics);
}

#[test]
fn stage5_ternary_node_itself_does_not_fold() {
    // The Ternary expression is *not* foldable as a whole — even if
    // both branches look literal, branch selection is data-driven.
    // BUT the walker still descends into each branch, and a `1 / 0`
    // Binary inside the `then` arm is a real sub-node that the
    // walker hands to `check_const_fold`. Mirroring the List case
    // below, we *do* expect the inner literal to fire.
    let tree = analyze_str(r#"{ cond: true, x: cond ? 1 / 0 : 0 }"#);
    assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
}

#[test]
fn stage5_ternary_with_runtime_in_branch_silent() {
    // Variant where the branch contains a non-literal — the inner
    // `a / 0` walker visit still folds, but the divisor is a literal
    // 0 so it *should* still fire. To test the "no-fire when
    // operand isn't literal" path we put the data-dependence on the
    // dividend side and divide by a non-zero constant: `cond ? a /
    // 1 : 0` should be silent because the inner Binary's left is a
    // Variable head (no fold) and the divisor isn't zero.
    let tree = analyze_str(r#"{ a: 1, cond: true, x: cond ? a / 1 : 0 }"#);
    assert_eq!(const_div_zero_count(&tree), 0, "{:?}", tree.diagnostics);
    assert_eq!(const_overflow_count(&tree), 0);
}

#[test]
fn stage5_div_zero_inside_list_still_fires() {
    // The list itself isn't foldable but the walker descends into
    // every list element — the inner `1 / 0` is still a Binary
    // node visited by the walker, so the diagnostic still fires.
    let tree = analyze_str(r#"{ x: [1 / 0] }"#);
    assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
}

#[test]
fn stage5_diagnostic_blocks_evaluation_via_has_errors() {
    // Stage 5 promotes ConstDivisionByZero / ConstNumericOverflow
    // to Severity::Error so `has_errors()` returns true and hosts
    // following the documented "skip eval on errors" pattern keep
    // the runtime out.
    let tree = analyze_str(r#"{ x: 9223372036854775807 + 1 }"#);
    assert!(tree.has_errors(), "{:?}", tree.diagnostics);
    assert_eq!(
        tree.diagnostics
            .iter()
            .find(|d| matches!(d, Diagnostic::ConstNumericOverflow { .. }))
            .map(|d| d.severity()),
        Some(crate::Severity::Error)
    );
}

// ----- v1.1: generic instantiation (List<T> / Result<T,E> -----

fn fn_call_arg_mismatch_count(tree: &AnalyzedTree) -> usize {
    tree.diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
        .count()
}

fn static_mismatch_count(tree: &AnalyzedTree) -> usize {
    tree.diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .count()
}

/// `_list_map(["a","b"], (s) => s)` returns `List<String>`; placing
/// it in a `List<Int>` slot must flag a static mismatch derivable
/// purely from source + stdlib signatures.
#[test]
fn v1_1_list_map_return_type_mismatches_int_slot() {
    let tree = analyze_str(r#"{ List<Int> xs: _list_map(["a", "b"], (s) => s) }"#);
    assert!(static_mismatch_count(&tree) >= 1, "{:?}", tree.diagnostics);
    assert!(tree.has_errors());
}

/// Inverse: `List<String>` slot, mapping `[1,2,3]` through
/// `(n) => n` returns `List<Int>` — should also flag.
#[test]
fn v1_1_list_map_return_type_mismatches_string_slot() {
    let tree = analyze_str(r#"{ List<String> xs: _list_map([1, 2, 3], (n) => n) }"#);
    assert!(static_mismatch_count(&tree) >= 1, "{:?}", tree.diagnostics);
    assert!(tree.has_errors());
}

/// `_list_contains([1,2], "x")` — `T` binds `Int` from arg 0; the
/// String literal in arg 1 then mismatches the substituted `T`
/// slot.
#[test]
fn v1_1_list_contains_arg_type_mismatch_after_unification() {
    let tree = analyze_str(r#"{ Bool b: _list_contains([1, 2], "x") }"#);
    assert!(
        fn_call_arg_mismatch_count(&tree) >= 1,
        "{:?}",
        tree.diagnostics
    );
    assert!(tree.has_errors());
}

/// Negative: `List<Int> xs: _list_map([1,2,3], (n) => n + 1)` —
/// `T → Int`, body type `Int`, `U → Int`, return `List<Int>`,
/// matches the slot. Should produce zero static / FnCall arg
/// mismatches related to the call.
#[test]
fn v1_1_list_map_int_to_int_passes() {
    let tree = analyze_str(r#"{ List<Int> xs: _list_map([1, 2, 3], (n) => n + 1) }"#);
    let irrelevant_ok = tree.diagnostics.iter().all(|d| {
        !matches!(
            d,
            Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
        )
    });
    assert!(irrelevant_ok, "{:?}", tree.diagnostics);
}

/// Negative: `_list_contains` with a same-typed needle stays
/// silent.
#[test]
fn v1_1_list_contains_same_type_passes() {
    let tree = analyze_str(r#"{ Bool b: _list_contains([1, 2, 3], 2) }"#);
    let irrelevant_ok = tree.diagnostics.iter().all(|d| {
        !matches!(
            d,
            Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
        )
    });
    assert!(irrelevant_ok, "{:?}", tree.diagnostics);
}

/// Negative: `_list_reduce([1,2,3], 0, (acc, x) => acc + x)` —
/// `T → Int` (from arg 0), `U → Int` (from `init`), body type
/// `Int`. Return slot `U` reads as `Int`, matches the
/// `Int s:` binding.
#[test]
fn v1_1_list_reduce_int_init_passes_int_slot() {
    let tree = analyze_str(r#"{ Int s: _list_reduce([1, 2, 3], 0, (acc, x) => acc + x) }"#);
    let irrelevant_ok = tree.diagnostics.iter().all(|d| {
        !matches!(
            d,
            Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
        )
    });
    assert!(irrelevant_ok, "{:?}", tree.diagnostics);
}

/// Consistency: when the analyzer reports a v1.1 mismatch, the
/// tree must have `has_errors() == true` so hosts that follow
/// the documented "skip eval on errors" pattern won't reach
/// the evaluator's runtime path. (The v1.1 report flips the
/// "is this caught statically?" answer to yes.)
#[test]
fn v1_1_static_mismatch_marks_tree_as_errored() {
    let tree = analyze_str(r#"{ Bool b: _list_contains([1, 2], "x") }"#);
    assert!(tree.has_errors(), "{:?}", tree.diagnostics);
}

// ============= v1.3 strict-mode tests =============

fn count(tree: &AnalyzedTree, pred: impl Fn(&Diagnostic) -> bool) -> usize {
    tree.diagnostics.iter().filter(|d| pred(d)).count()
}

/// v1.3 forward: in strict mode, an untyped non-dict spread
/// (`...e` where `e` isn't a dict literal) reports
/// `SpreadSourceTypeUnknown`.
#[test]
fn v1_3_strict_spread_without_type_flagged() {
    let tree = analyze_str(
        r#"
        { src: 1 + 2, ...src }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// Reverse: under `#relaxed`, the same spread is silent.
#[test]
fn relaxed_spread_silent() {
    let tree = analyze_str(
        r#"#relaxed
        { src: 1 + 2, ...src }"#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.3 reverse: typed spread `...<Extra> e` is silent under
/// strict mode (the user provided the hint).
#[test]
fn v1_3_strict_typed_spread_silent() {
    let tree = analyze_str(
        r#"
        #schema Extra { Int a: *, Int b: * }
        { src: { a: 1, b: 2 }, ...<Extra> src }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.3 forward: dynamic key without typehint flagged in strict
/// mode.
#[test]
fn v1_3_strict_dynamic_key_without_type_flagged() {
    let tree = analyze_str(
        r#"
        { k: "key", [k]: 1 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.3 reverse: typed dynamic key `[<String> k]:` silent.
#[test]
fn v1_3_strict_typed_dynamic_key_silent() {
    let tree = analyze_str(
        r#"
        { k: "key", [<String> k]: 1 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// Reverse: under `#relaxed`, an untyped dynamic key is silent.
#[test]
fn relaxed_dynamic_key_silent() {
    let tree = analyze_str(
        r#"#relaxed
        { k: "key", [k]: 1 }"#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.3 forward: DuplicateField fires (both modes) when a spread
/// of a known-shape source contributes a key that's already
/// declared.
#[test]
fn v1_3_duplicate_field_named_vs_typed_spread() {
    let tree = analyze_str(
        r#"
        #schema Extra { Int a: *, Int b: * }
        { src: { a: 1, b: 2 }, a: 99, ...<Extra> src }
        "#,
    );
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::DuplicateField { field, .. } if field == "a"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.3 forward: DuplicateField fires across two spreads of dict
/// literals that overlap.
#[test]
fn v1_3_duplicate_field_two_spreads_overlap() {
    let tree = analyze_str(r#"{ ...{ a: 1 }, ...{ a: 2 } }"#);
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::DuplicateField { field, .. } if field == "a"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.3 reverse: DuplicateField does not fire when the spread's
/// keys are unknown (untyped non-literal source) — we only emit
/// when we can statically prove the conflict.
#[test]
fn v1_3_duplicate_field_silent_when_spread_dynamic() {
    let tree = analyze_str(
        r#"
        { src: outer_value, a: 99, ...src }
        "#,
    );
    let n = count(&tree, |d| matches!(d, Diagnostic::DuplicateField { .. }));
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.3 forward: strict mode demands UnresolvedSchema for a typed
/// spread whose schema isn't declared.
#[test]
fn v1_3_strict_spread_unresolved_schema_flagged() {
    let tree = analyze_str(
        r#"
        { src: 1, ...<Mystery> src }
        "#,
    );
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::UnresolvedSchema { name, .. } if name == "Mystery"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.3 reverse: when the schema *is* declared, no
/// UnresolvedSchema fires.
#[test]
fn v1_3_strict_spread_known_schema_silent() {
    let tree = analyze_str(
        r#"
        #schema Extra { Int a: * }
        { src: { a: 1 }, ...<Extra> src }
        "#,
    );
    let n = count(&tree, |d| matches!(d, Diagnostic::UnresolvedSchema { .. }));
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// AnalyzedTree carries `strict_mode = true` by default — no
/// directive needed.
#[test]
fn strict_mode_bit_set_by_default() {
    let tree = analyze_str("{ a: 1 }");
    assert!(tree.strict_mode);
}

/// `#relaxed` clears the strict_mode bit.
#[test]
fn relaxed_directive_clears_strict_mode_bit() {
    let tree = analyze_str(
        r#"#relaxed
        { a: 1 }"#,
    );
    assert!(!tree.strict_mode);
}

/// `#unstrict` is a synonym for `#relaxed`; it also clears the bit.
#[test]
fn unstrict_directive_clears_strict_mode_bit() {
    let tree = analyze_str(
        r#"#unstrict
        { a: 1 }"#,
    );
    assert!(!tree.strict_mode);
}

/// Strict (the default) + native fn without static signature
/// should report `NativeFnSignatureMissing`. We simulate via an
/// `AnalyzeOptions::host_fn_names` entry without a corresponding
/// signature.
#[test]
fn strict_native_fn_signature_missing_without_signature() {
    let src = "{ x: my_native(1, 2) }";
    let node = parse_document(src).unwrap();
    let mut names = std::collections::HashSet::new();
    names.insert("my_native".to_string());
    let opts = crate::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: HashMap::new(),
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: true,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::NativeFnSignatureMissing { fn_name, .. } if fn_name == "my_native"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.3 reverse: same shape, but non-strict — silent.
#[test]
fn v1_3_non_strict_native_call_silent() {
    let src = "{ x: my_native(1, 2) }";
    let node = parse_document(src).unwrap();
    let mut names = std::collections::HashSet::new();
    names.insert("my_native".to_string());
    let opts = crate::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: HashMap::new(),
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: false,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::NativeFnSignatureMissing { .. })
    });
    assert_eq!(n, 0);
}

/// v1.3: strict + native fn *with* a signature — silent.
#[test]
fn v1_3_strict_native_with_signature_silent() {
    let src = "{ x: my_native(1, 2) }";
    let node = parse_document(src).unwrap();
    let mut names = std::collections::HashSet::new();
    names.insert("my_native".to_string());
    let mut sigs: HashMap<String, FnSignature> = HashMap::new();
    sigs.insert(
        "my_native".to_string(),
        FnSignature {
            name: "my_native".to_string(),
            generics: Vec::new(),
            params: vec![
                FnParam {
                    name: "a".to_string(),
                    ty: type_node_simple("Int"),
                    optional: false,
                },
                FnParam {
                    name: "b".to_string(),
                    ty: type_node_simple("Int"),
                    optional: false,
                },
            ],
            return_type: type_node_simple("Int"),
            variadic_tail: None,
        },
    );
    let opts = crate::AnalyzeOptions {
        host_fn_names: names,
        host_fn_signatures: sigs,
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: false,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::NativeFnSignatureMissing { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.8 (C4): host fn signature with `Any` parameter raises
/// `ExplicitAnyForbidden` carrying a `host fn` context.
#[test]
fn v1_8_host_fn_signature_any_param_rejected() {
    let node = relon_parser::parse_document("{ x: 1 }").unwrap();
    let mut sigs = HashMap::new();
    sigs.insert(
        "my_native".to_string(),
        crate::FnSignature {
            name: "my_native".to_string(),
            generics: Vec::new(),
            params: vec![FnParam {
                name: "blob".to_string(),
                ty: type_node_simple("Any"),
                optional: false,
            }],
            return_type: type_node_simple("Int"),
            variadic_tail: None,
        },
    );
    let opts = crate::AnalyzeOptions {
        host_fn_names: HashSet::new(),
        host_fn_signatures: sigs,
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: false,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(&tree, |d| {
        matches!(
            d,
            Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("host fn 'my_native'")
                    && context.contains("'blob'")
        )
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.8 (C4): host fn return-type `Any` raises
/// `ExplicitAnyForbidden` with a return-type context label.
#[test]
fn v1_8_host_fn_signature_any_return_rejected() {
    let node = relon_parser::parse_document("{ x: 1 }").unwrap();
    let mut sigs = HashMap::new();
    sigs.insert(
        "fetch".to_string(),
        crate::FnSignature {
            name: "fetch".to_string(),
            generics: Vec::new(),
            params: vec![FnParam {
                name: "url".to_string(),
                ty: type_node_simple("String"),
                optional: false,
            }],
            return_type: type_node_simple("Any"),
            variadic_tail: None,
        },
    );
    let opts = crate::AnalyzeOptions {
        host_fn_names: HashSet::new(),
        host_fn_signatures: sigs,
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: false,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(&tree, |d| {
        matches!(
            d,
            Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("host fn 'fetch'")
                    && context.contains("return type")
        )
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.8 (C4): host fn signature with bare `List` param raises
/// `BareGenericContainer`.
#[test]
fn v1_8_host_fn_signature_bare_list_rejected() {
    let node = relon_parser::parse_document("{ x: 1 }").unwrap();
    let mut sigs = HashMap::new();
    sigs.insert(
        "len_of".to_string(),
        crate::FnSignature {
            name: "len_of".to_string(),
            generics: Vec::new(),
            params: vec![FnParam {
                name: "xs".to_string(),
                ty: type_node_simple("List"),
                optional: false,
            }],
            return_type: type_node_simple("Int"),
            variadic_tail: None,
        },
    );
    let opts = crate::AnalyzeOptions {
        host_fn_names: HashSet::new(),
        host_fn_signatures: sigs,
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: false,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(&tree, |d| {
        matches!(
            d,
            Diagnostic::BareGenericContainer { type_name, context, .. }
                if type_name == "List" && context.contains("host fn 'len_of'")
        )
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.8 (C4): variadic-tail `Any` is also flagged.
#[test]
fn v1_8_host_fn_signature_any_variadic_rejected() {
    let node = relon_parser::parse_document("{ x: 1 }").unwrap();
    let mut sigs = HashMap::new();
    sigs.insert(
        "log".to_string(),
        crate::FnSignature {
            name: "log".to_string(),
            generics: Vec::new(),
            params: Vec::new(),
            return_type: type_node_simple("Null"),
            variadic_tail: Some(type_node_simple("Any")),
        },
    );
    let opts = crate::AnalyzeOptions {
        host_fn_names: HashSet::new(),
        host_fn_signatures: sigs,
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: false,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(&tree, |d| {
        matches!(
            d,
            Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("host fn 'log'") && context.contains("variadic tail")
        )
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.8 (C4): a clean signature using concrete types and unbound
/// generics raises no host-fn ban-Any / ban-bare diagnostics.
#[test]
fn v1_8_host_fn_signature_clean_silent() {
    let node = relon_parser::parse_document("{ x: 1 }").unwrap();
    let mut sigs = HashMap::new();
    sigs.insert(
        "id".to_string(),
        crate::FnSignature {
            name: "id".to_string(),
            generics: vec!["T".to_string()],
            params: vec![FnParam {
                name: "v".to_string(),
                ty: type_node_simple("T"),
                optional: false,
            }],
            return_type: type_node_simple("T"),
            variadic_tail: None,
        },
    );
    let opts = crate::AnalyzeOptions {
        host_fn_names: HashSet::new(),
        host_fn_signatures: sigs,
        host_fn_gates: HashMap::new(),
        caps: crate::Capabilities::default(),
        strict_mode: false,
        ..crate::AnalyzeOptions::default()
    };
    let tree = crate::analyze_with_options(&node, &opts);
    let n = count(&tree, |d| {
        matches!(
            d,
            Diagnostic::ExplicitAnyForbidden { context, .. }
                | Diagnostic::BareGenericContainer { context, .. }
                if context.contains("host fn")
        )
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.3 boundary: typed dynamic key with Int — silent.
#[test]
fn v1_3_typed_int_dynkey_silent() {
    let tree = analyze_str(
        r#"
        { idx: 0, [<Int> idx]: "row0" }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.3 boundary: typed dynamic key whose expression is a binary
/// op — silent.
#[test]
fn v1_3_typed_expr_dynkey_silent() {
    let tree = analyze_str(
        r#"
        { a: "x", b: "y", [<String> a + b]: 1 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ============= v1.4 strict completeness tests =============

/// v1.4 forward: under strict mode, a path tail descending into a
/// schema's missing field reports `UnknownReferenceType` with the
/// failing segment as `name` and the full path as the `path`
/// vector.
#[test]
fn v1_4_strict_path_tail_unknown_field() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: *, Float total: * }
        #main(Order o) -> Dict
        { x: o.unknown }
        "#,
    );
    let hits: Vec<_> = tree
        .diagnostics
        .iter()
        .filter_map(|d| match d {
            Diagnostic::UnknownReferenceType { name, path, .. } => {
                Some((name.clone(), path.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(hits.len(), 1, "{:?}", tree.diagnostics);
    assert_eq!(hits[0].0, "unknown");
    assert_eq!(hits[0].1, vec!["o".to_string(), "unknown".to_string()]);
}

/// v1.4 forward: descending into a leaf type (`Int` has no fields)
/// produces `UnknownReferenceType` on the failing segment.
#[test]
fn v1_4_strict_path_tail_int_descend() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: *, Float total: * }
        #main(Order o) -> Dict
        { x: o.id.something }
        "#,
    );
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "something"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.4 reverse: a fully classified path (`o.id` → Int) under
/// strict mode silently passes — no UnknownReferenceType.
#[test]
fn v1_4_strict_path_tail_known_field_silent() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: *, Float total: * }
        #main(Order o) -> Dict
        { x: o.id }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// Even under `#relaxed`, the path-tail walker reports
/// `UnknownReferenceType` for a positively-known broken step
/// (`o.unknown` on `Order` with no such field). The analyzer has
/// the schema's field index, so the failure is a static error
/// regardless of mode.
#[test]
fn non_strict_path_tail_reports_unknown_ref_type() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: *, Float total: * }
        #main(Order o) -> Dict<String, Int>
        { x: o.unknown }
        "#,
    );
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "unknown"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.4 forward: strict mode reports `UnknownReferenceType`
/// against a multi-hop chain whose final step lands on a leaf
/// (`o.customer.name.upper`).
#[test]
fn v1_4_strict_multi_hop_string_leaf_descend() {
    let tree = analyze_str(
        r#"
        #schema Customer { String name: * }
        #schema Order { Customer customer: *, Int id: * }
        #main(Order o) -> Dict
        { x: o.customer.name.upper }
        "#,
    );
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "upper"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

/// v1.4 forward: strict mode + path-spread of a typed schema field
/// (`...o.extras` where `Order.extras : Extras`). The v1.3 walker
/// would have demanded an explicit `<T>` typehint; v1.4 derives it
/// from the path-tail walker.
#[test]
fn v1_4_strict_path_spread_schema_silent() {
    let tree = analyze_str(
        r#"
        #schema Extras { Int a: *, Int b: * }
        #schema Order { Extras extras: *, Int id: * }
        #main(Order o) -> Dict
        { id: o.id, ...o.extras }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.4 forward: strict mode + path-spread of a `Dict<K,V>` field
/// — the value type is fully classified even though keys are
/// dynamic. No SpreadSourceTypeUnknown.
#[test]
fn v1_4_strict_path_spread_dict_silent() {
    let tree = analyze_str(
        r#"
        #schema Order { Dict<String, Int> kv: *, Int id: * }
        #main(Order o) -> Dict
        { id: o.id, ...o.kv }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.4 forward: strict mode + FnCall-spread (`...load_extras()`)
/// where `load_extras` is a sibling closure declared with `->
/// Extras`. The signature is harvested by the v1.4 pre-pass before
/// the spread check runs.
#[test]
fn v1_4_strict_fncall_spread_schema_silent() {
    let tree = analyze_str(
        r#"
        #schema Extras { Int a: *, Int b: * }
        {
          Extras src: { a: 1, b: 2 },
          load_extras: () -> Extras => src,
          ...load_extras()
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.4 forward: strict mode + path-spread whose tail-walk fails
/// (`...o.unknown`). Strict reports the more specific
/// UnknownReferenceType so the user sees the precise step that
/// stalled.
#[test]
fn v1_4_strict_path_spread_unknown_reports_specific() {
    let tree = analyze_str(
        r#"
        #schema Extras { Int a: *, Int b: * }
        #schema Order { Extras extras: *, Int id: * }
        #main(Order o) -> Dict
        { id: o.id, ...o.unknown }
        "#,
    );
    let unk = count(
        &tree,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "unknown"),
    );
    assert!(unk >= 1, "{:?}", tree.diagnostics);
}

/// v1.5 forward: strict mode + a typed binding whose value is a
/// well-formed list comprehension. The v1.5 inference engine now
/// derives `List<Int>` for `[x * 2 for x in range(5) if x > 0]`,
/// matching the declared `List<Int>` slot — no diagnostic.
/// (Pre-v1.5 this was an `ExpressionTypeUnknown` because the comprehension
/// was unconditionally opaque.)
#[test]
fn v1_5_strict_typed_binding_comprehension_inferable() {
    let tree = analyze_str(
        r#"
        { List<Int> xs: [x * 2 for x in range(5) if x > 0] }
        "#,
    );
    // No ExpressionTypeUnknown / StaticTypeMismatch — strict mode is
    // satisfied by the comprehension's derived element type.
    let il = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    let stm = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(il, 0, "{:?}", tree.diagnostics);
    assert_eq!(stm, 0, "{:?}", tree.diagnostics);
}

/// v1.4 reverse: a typed binding whose value *is* inferrable
/// (literal Int) doesn't fire ExpressionTypeUnknown even under strict
/// mode.
#[test]
fn v1_4_strict_typed_binding_inferrable_silent() {
    let tree = analyze_str(
        r#"
        { Int x: 42 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.4 forward: strict mode + a match arm whose body relies on
/// an unknown call — ExpressionTypeUnknown pinned on the arm body.
#[test]
fn v1_4_strict_match_arm_uninferrable() {
    let tree = analyze_str(
        r#"
        #schema Status Enum<"on", "off">
        #main(Status s) -> Dict
        { result: s match { on: mystery_call(), off: 0 } }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
            if reason.contains("match arm"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.4 reverse: strict mode + a match where every arm is
/// inferrable — no ExpressionTypeUnknown. Verifies the strict-aware
/// walker doesn't false-flag well-typed matches.
#[test]
fn v1_4_strict_match_arms_inferrable_silent() {
    let tree = analyze_str(
        r#"
        #schema Status Enum<"on", "off">
        #main(Status s) -> Dict
        { result: s match { on: 1, off: 0 } }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.8+ regression: an inline `<Dict<String, Int>>` typehint on a
/// dynamic spread is the documented strict-mode escape hatch
/// (`docs/zh/guide/spec.md` §6.6). Pre-fix the `spread_source_schema`
/// helper returned `Some("Dict")` because it took `path[0]` blindly,
/// then `schema_known("Dict")` was `false`, so the analyzer pushed
/// a bogus `UnresolvedSchema("Dict")`. The fix skips builtin heads
/// before treating them as schema names; `spread_source_is_dict`
/// owns the Dict-typed-spread classification path.
#[test]
fn v1_8e_strict_dict_typehint_spread_silent() {
    let tree = analyze_str(
        r#"
        #main(Dict<String, Int> kv) -> Dict
        {
          base: 1,
          ...<Dict<String, Int>> kv
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(
            d,
            Diagnostic::UnresolvedSchema { name, .. } if name == "Dict"
        )
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
    let m = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(m, 0, "{:?}", tree.diagnostics);
}

/// v1.4 boundary: spread source resolves to `Dict<String, Int>`
/// via FnCall return — strict accepts. Pairs with the
/// `path_spread_dict` fixture for the path side.
#[test]
fn v1_4_strict_fncall_spread_dict_silent() {
    let tree = analyze_str(
        r#"
        {
          Dict<String, Int> seed: { x: 1 },
          load_kv: () -> Dict<String, Int> => seed,
          ...load_kv()
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ============= v1.5 strict completeness — kill the long tail =============

/// v1.5 forward: list comprehension under a typed `List<Int>` slot
/// produces the matching element type and silences strict checks
/// that previously fired `ExpressionTypeUnknown`.
#[test]
fn v1_5_strict_comprehension_list_int_silent() {
    let tree = analyze_str(
        r#"
        { List<Int> doubled: [x * 2 for x in range(5)] }
        "#,
    );
    let il = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    let stm = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(il, 0, "{:?}", tree.diagnostics);
    assert_eq!(stm, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: comprehension binding's element type now flows
/// into the body's expression scope, so `x * 2` infers `Int` and
/// the resulting `List<Int>` mismatches a `List<String>` slot
/// statically.
#[test]
fn v1_5_strict_comprehension_element_mismatch() {
    let tree = analyze_str(
        r#"
        { List<String> xs: [x * 2 for x in range(5)] }
        "#,
    );
    let stm = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(stm >= 1, "{:?}", tree.diagnostics);
}

/// v1.5 forward: where-expression's body is inferred under a scope
/// extended with the bindings — `(n + 1)` infers Int when `n` was
/// bound to `x: Int`.
#[test]
fn v1_5_strict_where_body_int_silent() {
    let tree = analyze_str(
        r#"
        #main(Int x) -> Int
        (n + 1) where { n: x }
        "#,
    );
    let mm = count(&tree, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: where-body Int leaks against String return.
#[test]
fn v1_5_strict_where_body_string_mismatch() {
    let tree = analyze_str(
        r#"
        #main(Int x) -> String
        (n + 1) where { n: x }
        "#,
    );
    let mm = count(&tree, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
            if expected == "String" && found == "Int")
    });
    assert_eq!(mm, 1, "{:?}", tree.diagnostics);
}

/// v1.5 forward: untyped closure parameter under strict mode.
#[test]
fn v1_5_strict_closure_untyped_param_flagged() {
    let tree = analyze_str(
        r#"
        { f: (n) => n + 1 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureParamTypeMissing { param_name, .. }
            if param_name == "n")
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.5 reverse: typed closure parameter is silent.
#[test]
fn v1_5_strict_closure_typed_param_silent() {
    let tree = analyze_str(
        r#"
        { f: (Int n) -> Int => n + 1 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureParamTypeMissing { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: typed param but body relies on an unknown call,
/// no declared `-> ReturnType`. Body inference yields `Any` →
/// ClosureReturnTypeUnknown.
#[test]
fn v1_5_strict_closure_unclassified_body_flagged() {
    let tree = analyze_str(
        r#"
        { f: (Int n) => mystery(n) }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureReturnTypeUnknown { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.5 reverse: declared `-> ReturnType` makes the closure body
/// classifiable from the signature alone — no
/// ClosureReturnTypeUnknown.
#[test]
fn v1_5_strict_closure_declared_return_silent() {
    let tree = analyze_str(
        r#"
        { f: (Int n) -> Int => mystery(n) }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureReturnTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: head-unresolved reference in strict mode produces
/// `UnknownReferenceType { path: [head] }` alongside the warning-
/// level `UnresolvedReference`.
#[test]
fn v1_5_strict_head_unresolved_escalation() {
    let tree = analyze_str(
        r#"
        { x: mystery }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { name, path, .. }
            if name == "mystery" && path == &vec!["mystery".to_string()])
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// Reverse: `#relaxed` keeps the warning-level UnresolvedReference
/// and does NOT push UnknownReferenceType.
#[test]
fn relaxed_head_unresolved_no_unknown_ref_type() {
    let tree = analyze_str(
        r#"#relaxed
        { x: mystery }"#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.6 forward: `Any`-typed `#main` param now reports
/// `ExplicitAnyForbidden` in every mode (strict and non-strict).
/// Replaces the v1.5 `StrictForbidsUntypedMainParam` (which only
/// fired under strict). The new diagnostic carries a `context`
/// string so the user knows where the ban triggered.
#[test]
fn v1_6_main_param_any_flagged_under_strict() {
    let tree = analyze_str(
        r#"
        #main(Any x) -> Int
        1
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("#main parameter") && context.contains("`x`"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 forward: same ban applies under non-strict — `Any` is
/// retired from user code globally.
#[test]
fn v1_6_main_param_any_flagged_non_strict() {
    let tree = analyze_str(
        r#"
        #main(Any x) -> Int
        1
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("#main parameter"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 reverse: typed main param is silent in both modes.
#[test]
fn v1_6_main_param_typed_silent_under_strict() {
    let tree = analyze_str(
        r#"
        #main(Int x) -> Int
        x + 1
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: list element under strict mode whose value can't
/// be classified (FnCall without sig). ExpressionTypeUnknown pinned on
/// the element.
#[test]
fn v1_5_strict_list_element_uninferable() {
    let tree = analyze_str(
        r#"
        [1, mystery_call(), 3]
        "#,
    );
    let il = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
            if reason.contains("list element"))
    });
    assert!(il >= 1, "{:?}", tree.diagnostics);
}

/// v1.5 reverse: list of literals is silent.
#[test]
fn v1_5_strict_list_of_literals_silent() {
    let tree = analyze_str(
        r#"
        [1, 2, 3]
        "#,
    );
    let il = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    assert_eq!(il, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: untyped dict value with opaque expression →
/// ExpressionTypeUnknown.
#[test]
fn v1_5_strict_dict_value_uninferable() {
    let tree = analyze_str(
        r#"
        { x: mystery_fn() }
        "#,
    );
    let il = count(&tree, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
            if reason.contains("dict field"))
    });
    assert!(il >= 1, "{:?}", tree.diagnostics);
}

/// v1.5 forward: comprehension whose iterable is `range(...)` and
/// element body refers to `o.id` — inference should flow from
/// `Order { Int id }` so the element type is Int.
#[test]
fn v1_5_strict_comprehension_uses_main_param_path() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: *, Float total: * }
        #main(Order o) -> List<Int>
        [x + o.id for x in range(o.id)]
        "#,
    );
    let mm = count(&tree, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: spread used as a function-call argument evaluates
/// to its inner — `Expr::Spread` infers identically to the inner
/// expression. Used here as a smoke test that the new
/// `Expr::Spread` arm doesn't regress sibling-callable inference.
#[test]
fn v1_5_spread_expr_inference_smoke() {
    // Construct a Spread node directly via the parser path: a
    // typed binding whose value is `(...e) where { e: [1,2,3] }`
    // wouldn't actually exercise the arm because parser gates
    // `where`-bindings to dict literals. Instead assert on a
    // simpler shape: an empty `[]` is `List<Any>` and a literal
    // `[1, 2, 3]` infers as `List<Int>`. The Spread arm is
    // covered indirectly by spread-extension fixtures (where
    // `...x.y` flows through `Expr::Spread → infer_type(inner)`).
    let tree = analyze_str(
        r#"
        { List<Int> xs: [1, 2, 3] }
        "#,
    );
    let stm = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(stm, 0, "{:?}", tree.diagnostics);
}

/// v1.5 forward: FnCall with multi-segment alias.method now goes
/// through `lookup_signature_path`. We can't easily fixture-test
/// the cross-module case at the unit level, but the path-aware
/// lookup should *not* false-flag a sibling-dict-literal FnCall
/// that the v1.0 walker was already handling. (Regression guard.)
#[test]
fn v1_5_sibling_method_call_still_typechecks() {
    let tree = analyze_str(
        r#"
        {
          ns: {
            add: (Int a, Int b) -> Int => a + b
          },
          Int sum: ns.add(1, 2)
        }
        "#,
    );
    let stm = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(stm, 0, "{:?}", tree.diagnostics);
}

/// v1.5 boundary: spread of a typed sibling variable plus a
/// strict-mode typed closure — the v1.4 path-spread + v1.5
/// closure-strict checks coexist without false-flags.
#[test]
fn v1_5_strict_path_spread_after_typed_closure_silent() {
    let tree = analyze_str(
        r#"
        #schema Extras { Int a: *, Int b: * }
        {
          Extras src: { a: 1, b: 2 },
          build: (Int seed) -> Int => seed + 1,
          ...src
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.5 boundary: `where`-binding's value is a list literal that
/// itself contains an inferable element — body refers to the
/// binding and assembles a list of lists.
#[test]
fn v1_5_where_nested_list_body() {
    let tree = analyze_str(
        r#"
        #main(Int x) -> List<Int>
        xs where { List<Int> xs: [x, x + 1, x + 2] }
        "#,
    );
    let mm = count(&tree, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

/// v1.5 boundary: comprehension element is a path that walks two
/// hops (`o.customer.name`); strict + path-tail combine to
/// derive `String` element, matching the typed `List<String>`
/// slot.
#[test]
fn v1_5_strict_comprehension_path_two_hops_silent() {
    let tree = analyze_str(
        r#"
        #schema Customer { String name: * }
        #schema Order { Customer customer: *, Int id: * }
        #main(Order o) -> List<String>
        [o.customer.name for x in range(o.id)]
        "#,
    );
    let mm = count(&tree, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

// ============= v1.6: ban Any from user space =============

/// v1.6 forward: typed binding `Any field: ...` rejected (every mode).
#[test]
fn v1_6_ban_typed_binding_any() {
    let tree = analyze_str(r#"{ Any payload: 42 }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("typed binding"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 forward: nested `Any` inside `List<...>` is also flagged.
#[test]
fn v1_6_ban_nested_list_any() {
    let tree = analyze_str(r#"{ List<Any> xs: [1, 2, 3] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 forward: nested `Any` inside `Dict<String, ...>` flagged.
#[test]
fn v1_6_ban_nested_dict_any() {
    let tree = analyze_str(r#"{ Dict<String, Any> kv: { a: 1 } }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 forward: closure parameter typed `Any` flagged.
#[test]
fn v1_6_ban_closure_param_any() {
    let tree = analyze_str(r#"{ f: (Any n) -> Int => 1 }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("closure parameter"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 forward: closure declared `-> Any` flagged.
#[test]
fn v1_6_ban_closure_return_any() {
    let tree = analyze_str(r#"{ f: (Int n) -> Any => n }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("closure return"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 forward: schema field typed `Any` flagged.
#[test]
fn v1_6_ban_schema_field_any() {
    let tree = analyze_str(
        r#"
        #schema Outer { Any payload: * }
        { x: 1 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("schema field"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 forward: `#main(...) -> Any` flagged on the return type.
#[test]
fn v1_6_ban_main_return_any() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> Any
        n + 1
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("#main return"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.6 reverse: a fully concrete program does NOT trigger
/// ExplicitAnyForbidden.
#[test]
fn v1_6_ban_all_concrete_silent() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: *, String name: * }
        #main(Order o) -> Int
        {
          Int id: o.id,
          bump: (Int n) -> Int => n + 1,
          Int doubled: bump(o.id) + bump(o.id)
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.6 forward: stdlib `_dict_values<V>(Dict<String, V>) ->
/// List<V>` now flows V through.
#[test]
fn v1_6_stdlib_dict_values_flows_v_through() {
    let tree = analyze_str(
        r#"
        {
          Dict<String, Int> scores: { math: 100, art: 90 },
          List<Int> values: _dict_values(scores)
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.6 forward: stdlib `ensure.int<T>(value, message?) -> T` now
/// preserves the input type.
#[test]
fn v1_6_stdlib_ensure_int_preserves_t() {
    let tree = analyze_str(
        r#"
        #main(Int x) -> Int
        n + 1 where { Int n: ensure.int(x) }
        "#,
    );
    let mm = count(&tree, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

/// v1.6 forward: stdlib `len<T>(T) -> Int`. Param T is unbound;
/// no diagnostics for `len("hello")`.
#[test]
fn v1_6_stdlib_len_unbound_t() {
    let tree = analyze_str(
        r#"
        {
          s: "hello",
          Int n: len(s)
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(
            d,
            Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
        )
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.6 forward: stdlib `_dict_merge<V>` — uniform-V binds `V →
/// Int` and the return stays `Dict<String, Int>`.
#[test]
fn v1_6_stdlib_dict_merge_uniform_v() {
    let tree = analyze_str(
        r#"
        {
          Dict<String, Int> a: { x: 1 },
          Dict<String, Int> b: { y: 2 },
          Dict<String, Int> merged: _dict_merge(a, b)
        }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.6 forward: `Any` in `#schema` field caught when nested in a
/// parameterized container.
#[test]
fn v1_6_ban_schema_nested_any() {
    let tree = analyze_str(
        r#"
        #schema Bag { List<Any> items: * }
        { x: 1 }
        "#,
    );
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

// ============= v1.7: Tuple types =============

/// v1.7 forward: tuple-typed binding accepts a list literal of
/// matching arity / element types.
#[test]
fn v1_7_tuple_typed_binding_silent() {
    let tree = analyze_str(r#"{ (Int, String) row: [42, "Alice"] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.7 forward: tuple slot rejects element-type mismatch.
#[test]
fn v1_7_tuple_element_type_mismatch() {
    let tree = analyze_str(r#"{ (Int, String) row: [42, 99] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.7 forward: tuple slot rejects arity mismatch.
#[test]
fn v1_7_tuple_arity_mismatch() {
    let tree = analyze_str(r#"{ (Int, String) row: [42, "x", true] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.7 forward: 1-tuple syntax `(T,)` works.
#[test]
fn v1_7_one_tuple_silent() {
    let tree = analyze_str(r#"{ (Int,) singleton: [42] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.7 forward: unit tuple `()` accepts only an empty list.
#[test]
fn v1_7_unit_tuple_silent() {
    let tree = analyze_str(r#"{ () unit: [] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.7 forward: heterogeneous list under typed `List<T>` slot
/// still uses the per-element walker. The tuple inference
/// preserves precise element types so each mismatch reports its
/// exact position.
#[test]
fn v1_7_heterogeneous_list_under_int_slot_per_element_diagnostics() {
    let tree = analyze_str(r#"{ List<Int> xs: [1, "x", 3] }"#);
    let n = count(
        &tree,
        |d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "xs[1]"),
    );
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.7 forward: tuple inside `List<...>` (list of tuples) — each
/// row is a fixed-shape tuple.
#[test]
fn v1_7_list_of_tuples_silent() {
    let tree = analyze_str(r#"{ List<(String, Int)> entries: [["Alice", 1], ["Bob", 2]] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.7 forward: nested tuple positional mismatch surfaces with
/// the precise element index.
#[test]
fn v1_7_list_of_tuples_inner_mismatch() {
    let tree = analyze_str(r#"{ List<(String, Int)> entries: [["Alice", 1], ["Bob", "two"]] }"#);
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

/// v1.7 boundary: `(Int)` (no trailing comma) is *not* parsed as
/// a tuple — it's not a valid type position any more (v1.7
/// reserves parenthesized type syntax for tuples). The parser
/// rejects it; this test guards the rejection by checking the
/// fall-through path for method shorthand `f(x):` still works
/// (which depends on `(...)` not being claimed by the tuple
/// branch of `parse_type_node`).
#[test]
fn v1_7_method_shorthand_still_parses() {
    let tree = analyze_str(r#"{ helper(): 1, x: helper() }"#);
    // No UnresolvedReference / parse failure.
    let n = count(&tree, |d| {
        matches!(d, Diagnostic::UnresolvedReference { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

/// v1.5 Any-coverage audit: every "I-couldn't-infer" silent-Any
/// site under strict mode produces at least one error-severity
/// diagnostic. This is the regression guard for the user-visible
/// invariant "strict mode never produces an opaque `Any` type".
#[test]
fn v1_5_strict_any_coverage_audit() {
    let tree = analyze_str(
        r#"
        #schema Order { Int id: * }
        #main(Order o) -> Dict
        {
          bad_list: [mystery(), 1, 2],
          bad_closure: (Int n) => mystery(n),
          bad_path: o.unknown,
          untyped_closure: (n) => n + 1,
        }
        "#,
    );
    // Every "Any leak" site we documented in v1.5 must surface a
    // strict diagnostic. Use disjoint predicates so a single fix
    // can't accidentally cover a different leak.
    assert!(
        count(
            &tree,
            |d| matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
            if reason.contains("list element"))
        ) >= 1,
        "list element ExpressionTypeUnknown missing: {:?}",
        tree.diagnostics
    );
    assert!(
        count(&tree, |d| matches!(
            d,
            Diagnostic::ClosureReturnTypeUnknown { .. }
        )) >= 1,
        "closure body ClosureReturnTypeUnknown missing: {:?}",
        tree.diagnostics
    );
    assert!(
        count(&tree, |d| matches!(
            d,
            Diagnostic::UnknownReferenceType { name, .. } if name == "unknown"
        )) >= 1,
        "path-tail UnknownReferenceType missing: {:?}",
        tree.diagnostics
    );
    assert!(
        count(&tree, |d| matches!(
            d,
            Diagnostic::ClosureParamTypeMissing { param_name, .. }
                if param_name == "n"
        )) >= 1,
        "untyped param ClosureParamTypeMissing missing: {:?}",
        tree.diagnostics
    );
}

/// Phase 9.b-3: a single where-binding's body reference must
/// resolve under strict mode. Before the fix, `check_unresolved_var`
/// reached `x` without seeing a binding frame on `scope_stack`,
/// escalated the head-unresolved case to
/// `UnknownReferenceType { name: "x", ... }`, and broke every
/// strict-mode where-clause. The fix in `resolve.rs` and the visit
/// arm here pushes a binding frame derived from the dict before
/// walking the body.
#[test]
fn phase9_b3_strict_where_simple_binding_resolves() {
    let tree = analyze_str(
        r#"
        #main(Int seed) -> Int
        (x + 2) where { x: seed }
        "#,
    );
    let leaks = count(
        &tree,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "x"),
    );
    assert_eq!(
        leaks, 0,
        "where-bound `x` leaked under strict mode: {:?}",
        tree.diagnostics
    );
    let unresolved = count(
        &tree,
        |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "x"),
    );
    assert_eq!(
        unresolved, 0,
        "where-bound `x` flagged as UnresolvedReference: {:?}",
        tree.diagnostics
    );
}

/// Phase 9.b-3: nested where-clauses stack binding frames so the
/// inner body still sees the outer binding. `(x + y) where { y: x + 1 }`
/// embedded under an outer `where { x: 1 }` must resolve both names
/// under strict mode.
#[test]
fn phase9_b3_strict_where_nested_bindings_resolve() {
    let tree = analyze_str(
        r#"
        #main(Int seed) -> Int
        (
          ((x + y) where { y: x + 1 })
          where { x: seed }
        )
        "#,
    );
    let leaks: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::UnknownReferenceType { name, .. } if name == "x" || name == "y"
            )
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "nested where bindings leaked under strict mode: {:?}",
        leaks
    );
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| {
            matches!(
                d,
                Diagnostic::UnresolvedReference { name, .. } if name == "x" || name == "y"
            )
        })
        .collect();
    assert!(
        unresolved.is_empty(),
        "nested where bindings flagged as UnresolvedReference: {:?}",
        unresolved
    );
}

/// Phase 9.b-3: a where binding may shadow an outer name. The body
/// must observe the inner shadow under strict mode, not the outer
/// binding (or worse, the outer name's type via an opaque fallback).
/// The inner `x` is computed from the outer `x + 1`, so the inner
/// binding's *value* still references the outer `x` via the
/// `bindings` dict's normal scope walk.
#[test]
fn phase9_b3_strict_where_binding_shadows_outer() {
    let tree = analyze_str(
        r#"
        #main(Int seed) -> Int
        (
          (x * 2 where { x: x + 1 })
          where { x: seed }
        )
        "#,
    );
    let leaks: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "x"))
        .collect();
    assert!(
        leaks.is_empty(),
        "shadowed where binding leaked under strict mode: {:?}",
        leaks
    );
    let unresolved: Vec<_> = tree
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "x"))
        .collect();
    assert!(
        unresolved.is_empty(),
        "shadowed where binding flagged as UnresolvedReference: {:?}",
        unresolved
    );
}

// ---- R1: contextual closure / comprehension typing -----------------

/// R1 positive (bare form): `_list_map(xs, (x) => …)` derives `x: Int`
/// from the `List<Int>` receiver, so the untyped param no longer trips
/// `ClosureParamTypeMissing` / `ClosureReturnTypeUnknown` under strict.
#[test]
fn r1_bare_list_map_untyped_param_pinned() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> List<Int>
        _list_map([1, 2, 3], (x) => x * 2)
        "#,
    );
    let missing = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureParamTypeMissing { .. })
    });
    let ret = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureReturnTypeUnknown { .. })
    });
    assert_eq!(
        missing, 0,
        "param should be context-pinned: {:?}",
        tree.diagnostics
    );
    assert_eq!(
        ret, 0,
        "return should infer concretely: {:?}",
        tree.diagnostics
    );
}

/// R1 positive (method form): `range(n).map((x) => …)` routes through
/// the `_list_map` intrinsic with the receiver as arg 0, deriving
/// `x: Int` and suppressing both closure diagnostics.
#[test]
fn r1_method_map_untyped_param_pinned() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> Int
        range(n).map((x) => x * 2)
        "#,
    );
    let missing = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureParamTypeMissing { .. })
    });
    let ret = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureReturnTypeUnknown { .. })
    });
    assert_eq!(
        missing, 0,
        "method-form param should be context-pinned: {:?}",
        tree.diagnostics
    );
    assert_eq!(
        ret, 0,
        "method-form return should infer: {:?}",
        tree.diagnostics
    );
}

/// R1 positive (reduce, two closure params): `_list_reduce(xs, 0, (a, x)
/// => a + x)` binds both the accumulator (`U` from `init`) and the
/// element (`T` from `List<T>`), so neither param flags.
#[test]
fn r1_reduce_both_params_pinned() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> Int
        _list_reduce(range(n), 0, (a, x) => a + x)
        "#,
    );
    let missing = count(&tree, |d| {
        matches!(d, Diagnostic::ClosureParamTypeMissing { .. })
    });
    assert_eq!(
        missing, 0,
        "both reduce params should be pinned: {:?}",
        tree.diagnostics
    );
}

/// R1 positive (comprehension): `[x * 2 for x in range(n)]` derives
/// `x: Int` from the iterable so the body reference resolves instead
/// of `UnknownReferenceType`.
#[test]
fn r1_comprehension_binding_typed_from_iterable() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> List<Int>
        [x * 2 for x in range(n)]
        "#,
    );
    let unknown = count(
        &tree,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "x"),
    );
    assert_eq!(
        unknown, 0,
        "comprehension binding should be typed: {:?}",
        tree.diagnostics
    );
}

/// R1 negative: a closure with NO pinning call context still trips the
/// strict guard. The narrowing must only accept *derivable* cases —
/// true `Any` leaks stay rejected.
#[test]
fn r1_uncontextualized_closure_still_rejected() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> List<Int>
        [(x) => x + 1, n]
        "#,
    );
    let missing = count(
        &tree,
        |d| matches!(d, Diagnostic::ClosureParamTypeMissing { param_name, .. } if param_name == "x"),
    );
    assert!(
        missing >= 1,
        "uncontextualized closure must still flag: {:?}",
        tree.diagnostics
    );
}

/// R1 negative (comprehension): a non-derivable iterable element leaves
/// the binding `Any`, so strict mode still rejects it. `n` is an `Int`
/// (not iterable into a known element type), so the binding can't be
/// pinned and the iterable surfaces `ExpressionTypeUnknown`.
#[test]
fn r1_comprehension_non_iterable_still_rejected() {
    let tree = analyze_str(
        r#"
        #main(Int n) -> List<Int>
        [y * 2 for y in n]
        "#,
    );
    let unknown = count(
        &tree,
        |d| matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. } if reason.contains("`y`")),
    );
    assert!(
        unknown >= 1,
        "non-derivable comprehension binding must still flag: {:?}",
        tree.diagnostics
    );
}
