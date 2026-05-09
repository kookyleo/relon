//! Integration tests for inferable expression forms: list comprehension
//! and `where` blocks (both promoted out of the v1.5 silent-fallback
//! corner).

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== comprehension ======

#[test]
fn fixture_comprehension_int_match() {
    let tree = analyze_fixture("comprehension/list_comp_int.relon");
    let il = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::InferenceLimit { .. })
    });
    let stm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(il, 0, "{:?}", tree.diagnostics);
    assert_eq!(stm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_comprehension_string_mismatch() {
    let tree = analyze_fixture("comprehension/list_comp_string_mismatch.relon");
    let stm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(stm >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_comprehension_main_return_match() {
    let tree = analyze_fixture("comprehension/list_comp_main_return.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_comprehension_list_comp_o_id() {
    let tree = analyze_fixture("comprehension/list_comp_o_id.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

// ====== where_expr ======

#[test]
fn fixture_where_int_body() {
    let tree = analyze_fixture("where_expr/where_int_body.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_where_string_mismatch() {
    let tree = analyze_fixture("where_expr/where_string_mismatch.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
            if expected == "String" && found == "Int")
    });
    assert_eq!(mm, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_where_typed_binding() {
    let tree = analyze_fixture("where_expr/where_typed_binding.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}
