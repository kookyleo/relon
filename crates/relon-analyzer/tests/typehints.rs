//! Integration tests for the spread / dynamic-key type-hint surface
//! and for `spread_extension` propagation through path / fncall heads.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== typehint_spread ======

#[test]
fn fixture_typehint_spread_from_main_param() {
    let tree = analyze_fixture("typehint_spread/from_main_param.relon");
    let strict_diags = count(&tree.diagnostics, |d| {
        matches!(
            d,
            Diagnostic::MissingSpreadTypeHint { .. } | Diagnostic::UnresolvedSchema { .. }
        )
    });
    assert_eq!(strict_diags, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_from_sibling_field() {
    let tree = analyze_fixture("typehint_spread/from_sibling_field.relon");
    let strict_diags = count(&tree.diagnostics, |d| {
        matches!(
            d,
            Diagnostic::MissingSpreadTypeHint { .. } | Diagnostic::UnresolvedSchema { .. }
        )
    });
    assert_eq!(strict_diags, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_from_dict_literal() {
    let tree = analyze_fixture("typehint_spread/from_dict_literal.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_strict_missing_hint() {
    let tree = analyze_fixture("typehint_spread/strict_missing_hint.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_strict_unknown_schema() {
    let tree = analyze_fixture("typehint_spread/strict_unknown_schema.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnresolvedSchema { name, .. } if name == "Mystery"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

// ====== typehint_dynkey ======

#[test]
fn fixture_typehint_dynkey_typed_string() {
    let tree = analyze_fixture("typehint_dynkey/typed_string_key.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_typed_int() {
    let tree = analyze_fixture("typehint_dynkey/typed_int_key.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_typed_expression() {
    let tree = analyze_fixture("typehint_dynkey/typed_expression_key.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_missing_strict() {
    let tree = analyze_fixture("typehint_dynkey/missing_hint_strict.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_non_strict_silent() {
    let tree = analyze_fixture("typehint_dynkey/non_strict_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ====== spread_extension ======

#[test]
fn fixture_spread_path_schema() {
    let tree = analyze_fixture("spread_extension/path_spread_schema.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_spread_path_dict() {
    let tree = analyze_fixture("spread_extension/path_spread_dict.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_spread_fncall_schema() {
    let tree = analyze_fixture("spread_extension/fncall_spread_schema.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_spread_unknown_path() {
    let tree = analyze_fixture("spread_extension/spread_unknown_path.relon");
    // Strict mode should report the more specific UnknownReferenceType
    // diagnostic on the failing path-tail step (`o.unknown`).
    let unk = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "unknown"),
    );
    assert!(unk >= 1, "{:?}", tree.diagnostics);
}
