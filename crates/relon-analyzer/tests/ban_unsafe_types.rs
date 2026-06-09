//! Integration tests for the bans on user-facing `Any` (v1.6) and on
//! the bare-generic shorthand `List` / `Dict` / `Closure` / `Fn`
//! without explicit generic arguments (v1.7).

use relon_analyzer::Diagnostic;

mod common;
use common::*;

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
