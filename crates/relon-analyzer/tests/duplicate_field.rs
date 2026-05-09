//! Integration tests for duplicate-field detection across named keys
//! and typed / dynamic spread overlaps.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

#[test]
fn fixture_dup_named_vs_typed_spread() {
    let tree = analyze_fixture("duplicate_field/named_vs_typed_spread.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::DuplicateField { field, .. } if field == "a"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dup_two_spread_overlap() {
    let tree = analyze_fixture("duplicate_field/two_spread_overlap.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::DuplicateField { field, .. } if field == "a"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dup_nested_spread_collision() {
    let tree = analyze_fixture("duplicate_field/nested_spread_collision.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::DuplicateField { field, .. } if field == "x"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dup_disjoint_silent() {
    let tree = analyze_fixture("duplicate_field/disjoint_spread_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::DuplicateField { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dup_dynamic_silent() {
    let tree = analyze_fixture("duplicate_field/dynamic_spread_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::DuplicateField { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}
