//! Integration tests over `tests/fixtures/`.
//!
//! Each fixture's leading comment declares the expected outcome. We
//! parse + analyze the file and assert on the diagnostic shape. Same
//! style as `v1_3_fixtures.rs` so the two stay readable side-by-side.

use relon_analyzer::{analyze, Diagnostic};
use relon_parser::parse_document;
use std::path::PathBuf;
use std::sync::Arc;

fn load_fixture(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fixture {rel}: {e}"))
}

fn analyze_fixture(rel: &str) -> Arc<relon_analyzer::AnalyzedTree> {
    let src = load_fixture(rel);
    let node = parse_document(&src).unwrap_or_else(|e| panic!("parse {rel}: {e}"));
    Arc::new(analyze(&node))
}

fn count<F: Fn(&Diagnostic) -> bool>(diags: &[Diagnostic], pred: F) -> usize {
    diags.iter().filter(|d| pred(d)).count()
}

// ====== path_tail (Goal 1) ======

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

// ====== strict_silent_fallback (Goal 2) ======

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

#[test]
fn fixture_strict_non_strict_silent() {
    let tree = analyze_fixture("strict_silent_fallback/non_strict_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_typed_binding_unknown() {
    // v1.5 reframe: list comprehension is now inferable, so the
    // previously expected InferenceLimit no longer fires. We assert
    // silence on both InferenceLimit and StaticTypeMismatch.
    let tree = analyze_fixture("strict_silent_fallback/strict_typed_binding_unknown.relon");
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
fn fixture_strict_match_arm_unknown() {
    let tree = analyze_fixture("strict_silent_fallback/strict_match_arm_unknown.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::InferenceLimit { .. })
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
        matches!(d, Diagnostic::InferenceLimit { .. })
    });
    let unk = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::UnknownReferenceType { .. })
    });
    assert_eq!(inf, 0, "{:?}", tree.diagnostics);
    assert_eq!(unk, 0, "{:?}", tree.diagnostics);
}

// ====== spread_extension (Goal 3) ======

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
