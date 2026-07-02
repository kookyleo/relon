//! Integration tests for cross-module (`pkg.SchemaName`) resolution:
//! the import index, alias-namespacing, dual-alias-same-path, two libs
//! exporting same name, and 2-segment unknown-tail diagnostics.

use relon_analyzer::Diagnostic;

mod common;
use common::*;

#[test]
fn fixture_cross_module_pkg_schema_silent() {
    let ws = analyze_fixture_workspace("cross_module", "entry_pkg_schema_silent.relon");
    let total: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .count();
    assert_eq!(total, 0, "{:#?}", ws.modules);
}

#[test]
fn fixture_cross_module_pkg_schema_mismatch() {
    let ws = analyze_fixture_workspace("cross_module", "entry_pkg_schema_mismatch.relon");
    let total: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .count();
    assert!(total >= 1, "{:#?}", ws.modules);
}

#[test]
fn fixture_cross_module_pkg_schema_in_main_param() {
    // v1.8 cross-module: a `#main(lib.User u)` parameter type
    // resolves through the import index and seeds the resolver
    // scope. The body's `u.name` reference picks up String — no
    // UnknownReferenceType diagnostic.
    let ws = analyze_fixture_workspace("cross_module", "entry_main_param.relon");
    let total: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::UnknownReferenceType { .. } | Diagnostic::StaticTypeMismatch { .. }
            )
        })
        .count();
    assert_eq!(total, 0, "{:#?}", ws.modules);
}

/// v1.8+ regression: strict + cross-module + path-tail through a
/// `pkg.Schema` parameter. Pre-fix the param type lifted to `Any`
/// because the lift sites in `infer.rs` didn't forward the workspace
/// import index, so `walk_path` saw an `Any` head and reported
/// `UnknownReferenceType` for `u.name`; meanwhile the
/// `MainReturnTypeMismatch` check skipped on `Any` body. The fixture
/// asserts BOTH halves — the absence of the false-positive AND the
/// presence of the real mismatch — so a regression in either
/// direction is caught.
#[test]
fn fixture_cross_module_strict_pkg_schema_field_mismatch() {
    let ws = analyze_fixture_workspace("cross_module", "strict_pkg_schema_field_mismatch.relon");
    let mismatches: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::MainReturnTypeMismatch { expected, found, .. }
                    if expected == "Int" && found == "String"
            )
        })
        .count();
    assert_eq!(mismatches, 1, "{:#?}", ws.modules);
    let unknown_refs: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::UnknownReferenceType { .. }))
        .count();
    assert_eq!(unknown_refs, 0, "{:#?}", ws.modules);
}

/// v1.8+ regression (issue 3): two libs both export `User` but with
/// different field sets. Without alias-namespacing the importer's
/// schema index would last-write-wins one of the two, and field
/// references through the loser's alias would falsely report
/// `UnknownReferenceType`. The fixture imports both libs and
/// references each side's distinguishing field in body / typed
/// binding to prove both schemas survived.
#[test]
fn fixture_cross_module_two_libs_same_schema_name() {
    let ws = analyze_fixture_workspace("cross_module", "two_libs_same_schema_name.relon");
    let unknown_refs: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::UnknownReferenceType { .. }))
        .count();
    assert_eq!(unknown_refs, 0, "{:#?}", ws.modules);
    let mismatches: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
        .count();
    assert_eq!(mismatches, 0, "{:#?}", ws.modules);
}

/// v1.8+ regression (issue 2): the same file `#import`ed twice with
/// two different aliases must surface both aliases' schema sets.
/// Pre-fix `seen_raw` keyed by `(importer, raw_path)` and dropped the
/// second pending import entirely, so its alias never made it into
/// the import index — `b.User` showed up as an unknown 2-segment
/// type.
#[test]
fn fixture_cross_module_dual_alias_same_path() {
    let ws = analyze_fixture_workspace("cross_module", "dual_alias_same_path_entry.relon");
    let unknown_types: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::UnknownTypeName { name, .. }
                    if name == "a.User" || name == "b.User"
            )
        })
        .count();
    assert_eq!(unknown_types, 0, "{:#?}", ws.modules);
}

/// Schema-rooted §J follow-up: a `pkg.value.field` path-tail walk
/// through an aliased import's root-level value field now resolves
/// without strict-mode noise. The fixture's `lib.alice.region` ends
/// in `String`, so a declared `#main() -> String` lands cleanly.
#[test]
fn fixture_cross_module_strict_pkg_value_path_ok() {
    let ws = analyze_fixture_workspace("cross_module", "strict_pkg_value_path_ok.relon");
    let stalls: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::ExpressionTypeUnknown { .. }
                    | Diagnostic::UnknownReferenceType { .. }
                    | Diagnostic::MainReturnTypeMismatch { .. }
            )
        })
        .count();
    assert_eq!(stalls, 0, "{:#?}", ws.modules);
}

/// Schema-rooted §J follow-up: the negative half — when the `pkg.value
/// .field` walk now succeeds and produces a concrete type, a declared
/// `#main() -> Int` mismatching the field's `String` should surface as
/// `MainReturnTypeMismatch`. Pre-fix the walker returned `UnknownHead`
/// and the strict-mode pass collapsed to `ExpressionTypeUnknown` instead of
/// the precise mismatch.
#[test]
fn fixture_cross_module_strict_pkg_value_path_mismatch() {
    let ws = analyze_fixture_workspace("cross_module", "strict_pkg_value_path_mismatch.relon");
    let mismatches: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::MainReturnTypeMismatch { expected, found, .. }
                    if expected == "Int" && found == "String"
            )
        })
        .count();
    assert_eq!(mismatches, 1, "{:#?}", ws.modules);
    // No silent fallback: `ExpressionTypeUnknown` must not fire — the walker
    // resolved the value-path concretely.
    let stalls: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::ExpressionTypeUnknown { .. }))
        .count();
    assert_eq!(stalls, 0, "{:#?}", ws.modules);
}

/// v1.8+ cross-module type validation: a 2-segment param type whose
/// alias is valid but whose tail isn't in the alias's exported
/// schemas. Pre-fix the analyzer accepted any `pkg.Wrong` silently
/// (`subsumes_with_imports` conservative-passed and
/// `unknown_type_diagnostic` only checked single-segment paths).
#[test]
fn fixture_cross_module_pkg_unknown_schema_in_main_param() {
    let ws = analyze_fixture_workspace("cross_module", "pkg_unknown_schema_in_main_param.relon");
    let unknown_types: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(
            |d| matches!(d, Diagnostic::UnknownTypeName { name, .. } if name == "lib.NoSuchSchema"),
        )
        .count();
    assert!(unknown_types >= 1, "{:#?}", ws.modules);
}

/// Schema-rooted §J follow-up (generics): an aliased import's
/// `Container<Int> c: ...` value should walk cleanly through
/// `pkg.c.value` to `Int` — the substitution `{T → Int}` is
/// induced by the value's declared type and applied to the
/// schema's `T value: *` field declaration. Pre-fix the walker
/// kept `T` un-substituted and the declared `#main() -> Int`
/// surfaced a phantom `MainReturnTypeMismatch { found: "T" }`.
#[test]
fn fixture_cross_module_strict_pkg_generic_value_path_ok() {
    let ws = analyze_fixture_workspace("cross_module", "strict_pkg_generic_value_path_ok.relon");
    let stalls: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::ExpressionTypeUnknown { .. }
                    | Diagnostic::UnknownReferenceType { .. }
                    | Diagnostic::MainReturnTypeMismatch { .. }
                    | Diagnostic::StaticTypeMismatch { .. }
            )
        })
        .count();
    assert_eq!(stalls, 0, "{:#?}", ws.modules);
}

/// Schema-rooted §J follow-up (nested generics): two-hop generic
/// substitution through a cross-module value path. `pkg.w.inner
/// .value` for `Wrapper<Int> w` whose schema declares
/// `Container<T> inner: *` should thread `T → Int` through both
/// hops. Verifies that the namespace re-qualification step rewrites
/// `Container<T>` (bare sibling reference in `pkg`'s namespace) to
/// `pkg.Container<Int>` before lifting, and that the new schema's
/// substitution map is rebuilt from the post-substitution generic
/// args.
#[test]
fn fixture_cross_module_strict_pkg_nested_generic_value_path_ok() {
    let ws = analyze_fixture_workspace(
        "cross_module",
        "strict_pkg_nested_generic_value_path_ok.relon",
    );
    let stalls: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::ExpressionTypeUnknown { .. }
                    | Diagnostic::UnknownReferenceType { .. }
                    | Diagnostic::MainReturnTypeMismatch { .. }
                    | Diagnostic::StaticTypeMismatch { .. }
            )
        })
        .count();
    assert_eq!(stalls, 0, "{:#?}", ws.modules);
}

/// Schema-rooted §J follow-up (generics): the negative half. With
/// substitution wired through the walker, `pkg.c.value` resolves
/// concretely to `Int`, so a declared `#main() -> String` surfaces
/// the precise `MainReturnTypeMismatch { expected: "String",
/// found: "Int" }` rather than a generic `ExpressionTypeUnknown` (pre-fix)
/// or a phantom `found: "T"` (mid-fix without substitution).
#[test]
fn fixture_cross_module_strict_pkg_generic_value_path_mismatch() {
    let ws = analyze_fixture_workspace(
        "cross_module",
        "strict_pkg_generic_value_path_mismatch.relon",
    );
    let mismatches: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::MainReturnTypeMismatch { expected, found, .. }
                    if expected == "String" && found == "Int"
            )
        })
        .count();
    assert_eq!(mismatches, 1, "{:#?}", ws.modules);
    let stalls: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::ExpressionTypeUnknown { .. }))
        .count();
    assert_eq!(stalls, 0, "{:#?}", ws.modules);
}

/// Cross-strict `Any` leak (soundness): a strict entry importing a
/// `#relaxed` lib must not let the lib's untyped-closure `Any` return
/// whitewash a concrete typed *call argument* slot. Pre-fix the top-of
/// `subsumes_with_imports` `Any` short-circuit accepted anything, so
/// `f(lib.blob("hello"))` type-checked clean and produced a wrong
/// runtime value. Now it raises `FnCallArgTypeMismatch`.
#[test]
fn fixture_cross_module_strict_any_leak_call_flagged() {
    let ws = analyze_fixture_workspace("cross_module", "strict_any_leak_call.relon");
    let mismatches: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::FnCallArgTypeMismatch { expected, found, .. }
                    if expected == "Int" && found == "Any"
            )
        })
        .count();
    assert_eq!(mismatches, 1, "{:#?}", ws.modules);
}

/// Cross-strict `Any` into a scalar typed binding (`Int x: ...`) is
/// intentionally NOT statically flagged: value-binding slots are
/// enforced by the runtime typed-slot check, so the analyzer keeps the
/// permissive `Any` pass to avoid rejecting runtime-safe uses of the
/// untyped `#relaxed` stdlib. Pins that the fail-closed gate does not
/// bleed onto the value-slot path (the gate lives on the call-arg
/// boundary, covered by `..._call_flagged`).
#[test]
fn fixture_cross_module_strict_any_leak_scalar_permitted() {
    let ws = analyze_fixture_workspace("cross_module", "strict_any_leak_scalar.relon");
    let flagged: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .count();
    assert_eq!(flagged, 0, "{:#?}", ws.modules);
}

/// Per-module strictness: the *same* `#relaxed`-lib `Any` flow inside a
/// `#relaxed` entry stays permissive. Pins that the fail-closed gate is
/// scoped to the *checking* module's strictness — a relaxed importer is
/// not tightened, so no arg / scalar mismatch fires.
#[test]
fn fixture_cross_module_relaxed_any_leak_permitted() {
    let ws = analyze_fixture_workspace("cross_module", "relaxed_any_leak.relon");
    let flagged: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| {
            matches!(
                d,
                Diagnostic::FnCallArgTypeMismatch { .. } | Diagnostic::StaticTypeMismatch { .. }
            )
        })
        .count();
    assert_eq!(flagged, 0, "{:#?}", ws.modules);
}
