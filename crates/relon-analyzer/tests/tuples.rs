//! Integration tests for structured tuple types (v1.7 replacement of
//! the `List`-as-tuple overload) and positional tuple-index access
//! (v1.8 `t.0` / `t.1` form).

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== tuple ======

#[test]
fn fixture_tuple_homogeneous_list_compatible() {
    let tree = analyze_fixture("tuple/homogeneous_list_compatible.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_heterogeneous_list_rejected() {
    let tree = analyze_fixture("tuple/heterogeneous_list_rejected.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_typed_tuple_exact() {
    let tree = analyze_fixture("tuple/typed_tuple_exact.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_typed_tuple_arity_mismatch() {
    let tree = analyze_fixture("tuple/typed_tuple_arity_mismatch.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_typed_tuple_position_mismatch() {
    let tree = analyze_fixture("tuple/typed_tuple_position_mismatch.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_empty_tuple() {
    let tree = analyze_fixture("tuple/empty_tuple.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_single_tuple() {
    let tree = analyze_fixture("tuple/single_tuple.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_nested_tuple() {
    let tree = analyze_fixture("tuple/nested_tuple.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_nested_tuple_inner_mismatch() {
    let tree = analyze_fixture("tuple/nested_tuple_inner_mismatch.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_heterogeneous_typed_tuple() {
    let tree = analyze_fixture("tuple/heterogeneous_typed_tuple.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ====== tuple_index (positional access) ======

#[test]
fn fixture_tuple_index_position_int_silent() {
    let tree = analyze_fixture("tuple_index/position_int_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_index_position_string_silent() {
    let tree = analyze_fixture("tuple_index/position_string_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_index_position_type_mismatch() {
    let tree = analyze_fixture("tuple_index/position_type_mismatch.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_index_out_of_range() {
    let tree = analyze_fixture("tuple_index/out_of_range.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_index_list_index_silent() {
    let tree = analyze_fixture("tuple_index/list_index_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ====== tuple_schema (named positional schema) ======

#[test]
fn fixture_tuple_schema_named_ipv4_index_silent() {
    let tree = analyze_fixture("tuple_schema/named_ipv4_index_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(
            d,
            Diagnostic::StaticTypeMismatch { .. } | Diagnostic::UnknownReferenceType { .. }
        )
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_schema_named_ipv4_return_silent() {
    let tree = analyze_fixture("tuple_schema/named_ipv4_return_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_schema_literal_into_named_schema_silent() {
    let tree = analyze_fixture("tuple_schema/tuple_literal_into_named_schema_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_schema_list_not_tuple_schema() {
    let tree = analyze_fixture("tuple_schema/list_not_tuple_schema.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_tuple_schema_position_mismatch() {
    let tree = analyze_fixture("tuple_schema/tuple_schema_position_mismatch.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}
