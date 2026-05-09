//! Integration tests over `tests/fixtures/`.
//!
//! v1.7 introduces structured tuple types (replacing the
//! `List`-as-tuple overload) and bans the bare-generic shorthand
//! (`List` / `Dict` / `Closure` / `Fn` / `Enum` without explicit
//! generic arguments). Each fixture's leading comment declares the
//! expected outcome — we re-state it as an assertion below.

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

// ====== ban_bare ======

#[test]
fn fixture_ban_bare_list_rejected() {
    let tree = analyze_fixture("ban_bare/bare_list_rejected.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::BareGenericContainer { type_name, .. } if type_name == "List"),
    );
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_bare_dict_rejected() {
    let tree = analyze_fixture("ban_bare/bare_dict_rejected.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::BareGenericContainer { type_name, .. } if type_name == "Dict"),
    );
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_bare_closure_rejected() {
    let tree = analyze_fixture("ban_bare/bare_closure_rejected.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::BareGenericContainer { type_name, .. } if type_name == "Closure"),
    );
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_bare_fn_rejected() {
    let tree = analyze_fixture("ban_bare/bare_fn_rejected.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::BareGenericContainer { type_name, .. } if type_name == "Fn"),
    );
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_bare_nested_list_rejected() {
    let tree = analyze_fixture("ban_bare/nested_bare_list_rejected.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::BareGenericContainer { type_name, .. } if type_name == "List"),
    );
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_bare_explicit_generics_silent() {
    let tree = analyze_fixture("ban_bare/explicit_generics_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::BareGenericContainer { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_ban_bare_main_param_rejected() {
    let tree = analyze_fixture("ban_bare/bare_main_param_rejected.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::BareGenericContainer { type_name, .. } if type_name == "List"),
    );
    assert!(n >= 1, "{:?}", tree.diagnostics);
}
