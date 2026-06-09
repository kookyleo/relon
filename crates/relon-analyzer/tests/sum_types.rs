//! Integration tests for `Result<Ok, Err>` variant-generic substitution.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== result (variant-generic substitution) ======

#[test]
fn fixture_result_ok_value_correct() {
    let tree = analyze_fixture("result/ok_value_correct.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_result_ok_value_mistyped() {
    let tree = analyze_fixture("result/ok_value_mistyped.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_result_err_field_correct() {
    let tree = analyze_fixture("result/err_field_correct.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_result_err_field_mistyped() {
    let tree = analyze_fixture("result/err_field_mistyped.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}
