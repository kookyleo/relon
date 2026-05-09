//! Integration tests for generic-typed `Dict` slots and the
//! stdlib-side generic placeholder substitution.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== dict_generics ======

#[test]
fn fixture_dict_generics_bare_compatible() {
    let tree = analyze_fixture("dict_generics/bare_dict_still_works.relon");
    assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_string_int() {
    let tree = analyze_fixture("dict_generics/dict_string_int.relon");
    assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_string_int_mismatch() {
    let tree = analyze_fixture("dict_generics/dict_string_int_mismatch.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "scores.art"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_nested_result() {
    let tree = analyze_fixture("dict_generics/dict_nested_result.relon");
    assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_int_list_string() {
    let tree = analyze_fixture("dict_generics/dict_int_list_string.relon");
    // Even if `Int` keys aren't structurally validated, the type slot
    // should parse and not blow up.
    let stt = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(stt, 0, "{:?}", tree.diagnostics);
}

// ====== stdlib_generic ======

#[test]
fn fixture_stdlib_dict_values_typed() {
    let tree = analyze_fixture("stdlib_generic/dict_values_typed.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_stdlib_ensure_int_returns_int() {
    let tree = analyze_fixture("stdlib_generic/ensure_int_returns_int.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}
