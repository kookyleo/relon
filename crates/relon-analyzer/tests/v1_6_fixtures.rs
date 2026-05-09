//! Integration tests over `tests/fixtures/`.
//!
//! v1.6 retires `Any` from the user-facing surface (every mode) and
//! replaces every stdlib-signature `Any` with an unbound generic
//! placeholder so the language surface no longer mentions it.
//!
//! Each fixture's leading comment declares the expected outcome.

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

// ====== ban_any ======

#[test]
fn fixture_ban_typed_binding_any() {
    let tree = analyze_fixture("ban_any/typed_binding_any.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("typed binding") && context.contains("payload"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_nested_list_any() {
    let tree = analyze_fixture("ban_any/nested_list_any.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_nested_dict_any() {
    let tree = analyze_fixture("ban_any/nested_dict_any.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_closure_param_any() {
    let tree = analyze_fixture("ban_any/closure_param_any.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("closure parameter"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_closure_return_any() {
    let tree = analyze_fixture("ban_any/closure_return_any.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("closure return"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_schema_field_any() {
    let tree = analyze_fixture("ban_any/schema_field_any.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("schema field"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_main_return_any() {
    let tree = analyze_fixture("ban_any/main_return_any.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
            if context.contains("#main return"))
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_all_concrete_silent() {
    let tree = analyze_fixture("ban_any/all_concrete_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
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
