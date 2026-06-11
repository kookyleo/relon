//! Integration tests for `#main` signature handling: root-injection of
//! `#main` parameters into atomic / dict / list / variant return slots,
//! and the strict-mode rules around untyped `#main` parameters.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

// ====== main_injection ======

#[test]
fn fixture_main_injection_atomic_int_return() {
    let tree = analyze_fixture("main_injection/atomic_root_int_return.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_main_injection_atomic_string_mismatch() {
    let tree = analyze_fixture("main_injection/atomic_root_string_mismatch.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
            if expected == "String" && found == "Int")
    });
    assert_eq!(mm, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_main_injection_dict_root_param() {
    let tree = analyze_fixture("main_injection/dict_root_field_uses_param.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    let unresolved = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "n"),
    );
    assert_eq!(unresolved, 0, "param `n` should be resolved");
}

#[test]
fn fixture_main_injection_list_root_param() {
    let tree = analyze_fixture("main_injection/list_root_uses_param.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_main_injection_variant_root_param() {
    let tree = analyze_fixture("main_injection/variant_root_uses_param.relon");
    let unresolved = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "n"),
    );
    assert_eq!(unresolved, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_main_injection_dict_root_field_access() {
    let tree = analyze_fixture("main_injection/dict_root_param_field_access.relon");
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
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
fn fixture_main_return_any_body_strict_flags_gap() {
    let tree = analyze_fixture("main_strict/main_return_any_strict.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
            if reason.contains("#main") && reason.contains("String"))
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
    let mm = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
    });
    assert_eq!(mm, 0, "{:?}", tree.diagnostics);
}

// ====== main_sig ======

#[test]
fn fixture_main_dup_param_flagged() {
    let tree = analyze_fixture("main_sig/main_dup_param.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::DuplicateMainParam { name, .. } if name == "x")
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}
