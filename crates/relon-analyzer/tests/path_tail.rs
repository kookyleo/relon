//! Integration tests for path-tail propagation: walking schema chains,
//! dict-value chains, optional-field stripping, and closure-param
//! field chains via `walk_path`.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

#[test]
fn fixture_path_tail_atomic_field_int_match() {
    let tree = analyze_fixture("path_tail/atomic_field_int_match.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_path_tail_atomic_field_string_mismatch() {
    let tree = analyze_fixture("path_tail/atomic_field_string_mismatch.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
            if expected == "String" && found == "Int")
    });
    assert_eq!(mm, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_path_tail_multi_hop_schema_chain() {
    let tree = analyze_fixture("path_tail/multi_hop_schema_chain.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_path_tail_dict_value_chain() {
    let tree = analyze_fixture("path_tail/dict_value_chain.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_path_tail_dict_value_chain_mismatch() {
    let tree = analyze_fixture("path_tail/dict_value_chain_mismatch.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
            if expected == "String" && found == "Int")
    });
    assert_eq!(mm, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_path_tail_optional_field_strip() {
    let tree = analyze_fixture("path_tail/optional_field_strip.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_path_tail_closure_param_chain() {
    let tree = analyze_fixture("path_tail/closure_param_field_chain.relon");
    // The closure body `o.id` produces Int and matches the declared
    // `-> Int`, so no static-type-mismatch diagnostics. We're not
    // strict here; only the closure return path is exercised.
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}
