//! Integration tests over `tests/fixtures/`.
//!
//! v1.5 closes the remaining strict-mode silent fallbacks: comprehension
//! / where / spread expressions are now inferable; closure params and
//! `#main` params must declare a type; head-unresolved variables are
//! escalated under strict mode; and FnCall multi-segment names route
//! through the path-aware signature lookup.

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

// ====== closure_strict ======

#[test]
fn fixture_closure_param_typed_silent() {
    let tree = analyze_fixture("closure_strict/closure_param_typed.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StrictForbidsUntypedClosureParam { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_closure_param_untyped_flags() {
    let tree = analyze_fixture("closure_strict/closure_param_untyped.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StrictForbidsUntypedClosureParam { param_name, .. }
            if param_name == "n")
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_closure_body_unclassified_flags() {
    let tree = analyze_fixture("closure_strict/closure_body_unclassified.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StrictForbidsUnclassifiedClosureBody { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
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

// ====== main_strict ======

#[test]
fn fixture_main_param_any_strict_flags() {
    // v1.6 reframe: the v1.5 `StrictForbidsUntypedMainParam` diagnostic
    // was retired in favor of the generic `ExplicitAnyForbidden` (which
    // fires in every mode). Same fixture asserts the new shape.
    let tree = analyze_fixture("main_strict/main_param_any_strict.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("#main parameter") && context.contains("`x`"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_main_param_typed_silent() {
    let tree = analyze_fixture("main_strict/main_param_typed.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_main_no_main_strict_silent() {
    let tree = analyze_fixture("main_strict/main_no_main_strict_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
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

#[test]
fn fixture_comprehension_list_comp_o_id() {
    let tree = analyze_fixture("comprehension/list_comp_o_id.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_where_typed_binding() {
    let tree = analyze_fixture("where_expr/where_typed_binding.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}
