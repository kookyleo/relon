//! Integration tests for sum-type slots: `Enum<...>` static subsumption
//! (each alternative checked, accept on any compatible match) and
//! `Result<Ok, Err>` variant-generic substitution.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== enum ======

#[test]
fn fixture_enum_string_alts_accept_string() {
    let tree = analyze_fixture("enum/string_alts_accept_string.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_string_alts_reject_int() {
    let tree = analyze_fixture("enum/string_alts_reject_int.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_heterogeneous_alts_reject_bool() {
    let tree = analyze_fixture("enum/heterogeneous_alts.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_heterogeneous_alts_int_ok() {
    let tree = analyze_fixture("enum/heterogeneous_alts_int_ok.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_numeric_alts_int_float() {
    let tree = analyze_fixture("enum/numeric_alts_int_float.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_list_in_enum_slot_rejected() {
    let tree = analyze_fixture("enum/list_in_enum_slot_rejected.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

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

#[test]
fn fixture_result_custom_enum_generic() {
    let tree = analyze_fixture("result/custom_enum_generic.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}
