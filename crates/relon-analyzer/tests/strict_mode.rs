//! Integration tests for strict-mode behavior: the strict bit, demands
//! around spread / dynamic-key type hints, silent-fallback semantics in
//! non-strict, head-unresolved escalation, closure parameter / body
//! constraints, and multi-module strict propagation.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== strict_basic ======

#[test]
fn fixture_strict_enables_bit() {
    let tree = analyze_fixture("strict_basic/strict_enables_bit.relon");
    assert!(tree.strict_mode);
}

#[test]
fn fixture_strict_demands_spread_hint() {
    let tree = analyze_fixture("strict_basic/strict_demands_spread_hint.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_demands_dynkey_hint() {
    let tree = analyze_fixture("strict_basic/strict_demands_dynkey_hint.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_typed_spread_silent() {
    let tree = analyze_fixture("strict_basic/strict_typed_spread_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_typed_dynkey_silent() {
    let tree = analyze_fixture("strict_basic/strict_typed_dynkey_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ====== strict_silent_fallback ======

#[test]
fn fixture_strict_unknown_field() {
    let tree = analyze_fixture("strict_silent_fallback/strict_unknown_field.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { name, path, .. }
            if name == "unknown" && path == &vec!["f".to_string(), "unknown".to_string()])
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_descend_into_int() {
    let tree = analyze_fixture("strict_silent_fallback/strict_descend_into_int.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "something"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_known_path_silent() {
    let tree = analyze_fixture("strict_silent_fallback/strict_known_path_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// (v1.4's `fixture_strict_non_strict_silent` retired in v2 — its
// premise was "non-strict stays silent on `o.unknown`", which v2
// inverts: the path-tail walker now fires `UnknownReferenceType`
// cross-mode whenever the analyzer has positive knowledge a step
// is broken. The replacement assertion lives in
// `typecheck::tests::non_strict_path_tail_reports_unknown_ref_type`.)

#[test]
fn fixture_strict_typed_binding_unknown() {
    // v1.5 reframe: list comprehension is now inferable, so the
    // previously expected ExpressionTypeUnknown no longer fires. We assert
    // silence on both ExpressionTypeUnknown and StaticTypeMismatch.
    let tree = analyze_fixture("strict_silent_fallback/strict_typed_binding_unknown.relon");
    let il = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    let stm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(il, 0, "{:?}", tree.diagnostics);
    assert_eq!(stm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_match_arm_unknown() {
    let tree = analyze_fixture("strict_silent_fallback/strict_match_arm_unknown.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_path_chain_int_descend() {
    let tree = analyze_fixture("strict_silent_fallback/strict_path_chain_int_descend.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "upper"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_typed_silent_inferable() {
    let tree = analyze_fixture("strict_silent_fallback/strict_typed_silent_inferable.relon");
    let inf = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
    });
    let unk = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert_eq!(inf, 0, "{:?}", tree.diagnostics);
    assert_eq!(unk, 0, "{:?}", tree.diagnostics);
}

// ====== strict_head ======

#[test]
fn fixture_strict_head_unresolved_escalates() {
    let tree = analyze_fixture("strict_head/strict_unresolved_head.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { name, path, .. }
            if name == "mystery" && path == &vec!["mystery".to_string()])
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_non_strict_head_unresolved_silent() {
    let tree = analyze_fixture("strict_head/non_strict_unresolved_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_path_after_unresolved_silent() {
    let tree = analyze_fixture("strict_head/strict_path_after_unresolved_silent.relon");
    let unk = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "range"),
    );
    let unr = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "range"),
    );
    assert_eq!(unk, 0, "{:?}", tree.diagnostics);
    assert_eq!(unr, 0, "{:?}", tree.diagnostics);
}

// ====== closure_strict ======

#[test]
fn fixture_closure_param_typed_silent() {
    let tree = analyze_fixture("closure_strict/closure_param_typed.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ClosureParamTypeMissing { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_closure_param_untyped_flags() {
    let tree = analyze_fixture("closure_strict/closure_param_untyped.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ClosureParamTypeMissing { param_name, .. }
            if param_name == "n")
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_closure_body_unclassified_flags() {
    let tree = analyze_fixture("closure_strict/closure_body_unclassified.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ClosureReturnTypeUnknown { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

// ====== strict_propagation (multi-module) ======

#[test]
fn fixture_strict_propagation_one_hop() {
    let ws = analyze_fixture_workspace("strict_propagation", "entry.relon");
    assert!(ws.strict_mode);
    for (id, tree) in &ws.modules {
        assert!(
            tree.strict_mode,
            "module {id} should be strict-tagged: {:?}",
            tree.diagnostics
        );
    }
    // The lib's silent-fallback spread should be reported.
    let total_spread_diags: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. }))
        .count();
    assert!(total_spread_diags >= 1, "expected lib's spread diag");
}

#[test]
fn fixture_strict_propagation_two_hops() {
    let ws = analyze_fixture_workspace("strict_propagation", "chain_entry.relon");
    assert!(ws.strict_mode);
    assert_eq!(ws.modules.len(), 3, "entry + mid + leaf");
    for (id, tree) in &ws.modules {
        assert!(tree.strict_mode, "{id}");
    }
}

#[test]
fn fixture_strict_propagation_diamond() {
    let ws = analyze_fixture_workspace("strict_propagation", "diamond_entry.relon");
    assert!(ws.strict_mode);
    assert_eq!(ws.modules.len(), 4, "entry + b + c + d");
    for (id, tree) in &ws.modules {
        assert!(tree.strict_mode, "{id}");
    }
}
