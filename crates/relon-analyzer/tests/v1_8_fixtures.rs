//! Integration tests over `tests/fixtures/v1_8/`.
//!
//! v1.8 turns `Enum<...>` into a first-class slot for static
//! subsumption: instead of unconditionally accepting any value, the
//! analyzer checks each alternative and only accepts when at least
//! one is statically compatible.

use relon_analyzer::{
    analyze, analyze_entry,
    workspace::{LoadError, LoadedModule, ModuleLoader},
    Diagnostic,
};
use relon_parser::parse_document;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn load_fixture(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v1_8")
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

// ====== enum ======

#[test]
fn fixture_enum_string_alts_accept_string() {
    let tree = analyze_fixture("enum/string_alts_accept_string.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_string_alts_reject_int() {
    let tree = analyze_fixture("enum/string_alts_reject_int.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_heterogeneous_alts_reject_bool() {
    let tree = analyze_fixture("enum/heterogeneous_alts.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_heterogeneous_alts_int_ok() {
    let tree = analyze_fixture("enum/heterogeneous_alts_int_ok.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_numeric_alts_int_float() {
    let tree = analyze_fixture("enum/numeric_alts_int_float.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_enum_list_in_enum_slot_rejected() {
    let tree = analyze_fixture("enum/list_in_enum_slot_rejected.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

// ====== result (variant-generic substitution) ======

#[test]
fn fixture_result_ok_value_correct() {
    let tree = analyze_fixture("result/ok_value_correct.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_result_ok_value_mistyped() {
    let tree = analyze_fixture("result/ok_value_mistyped.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_result_err_field_correct() {
    let tree = analyze_fixture("result/err_field_correct.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_result_err_field_mistyped() {
    let tree = analyze_fixture("result/err_field_mistyped.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_result_custom_enum_generic() {
    let tree = analyze_fixture("result/custom_enum_generic.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

// ====== cross_module (pkg.SchemaName) ======

struct DiskLoader {
    root: PathBuf,
    canonical: HashMap<String, String>,
}

impl DiskLoader {
    fn new(rel_dir: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/v1_8")
            .join(rel_dir);
        Self {
            root,
            canonical: HashMap::new(),
        }
    }
}

impl ModuleLoader for DiskLoader {
    fn load(&mut self, path: &str, _current_dir: &Path) -> Result<LoadedModule, LoadError> {
        let p = self.root.join(path.trim_start_matches("./"));
        let canonical_id = p.to_string_lossy().to_string();
        let source = std::fs::read_to_string(&p).map_err(|_| LoadError::NotFound)?;
        self.canonical
            .insert(path.to_string(), canonical_id.clone());
        Ok(LoadedModule {
            canonical_id,
            source,
            current_dir: self.root.clone(),
        })
    }
}

fn analyze_cross_module_fixture(sub_dir: &str, entry_rel: &str) -> relon_analyzer::WorkspaceTree {
    let mut loader = DiskLoader::new(sub_dir);
    let entry_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v1_8")
        .join(sub_dir)
        .join(entry_rel);
    let src = std::fs::read_to_string(&entry_path).unwrap();
    analyze_entry(
        entry_path.to_string_lossy().to_string(),
        &src,
        entry_path.parent().unwrap().to_path_buf(),
        &mut loader,
    )
}

#[test]
fn fixture_cross_module_pkg_schema_silent() {
    let ws = analyze_cross_module_fixture("cross_module", "entry_pkg_schema_silent.relon");
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
    let ws = analyze_cross_module_fixture("cross_module", "entry_pkg_schema_mismatch.relon");
    let total: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .count();
    assert!(total >= 1, "{:#?}", ws.modules);
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

#[test]
fn fixture_cross_module_pkg_schema_in_main_param() {
    // v1.8 cross-module: a `#main(lib.User u)` parameter type
    // resolves through the import index and seeds the resolver
    // scope. The body's `u.name` reference picks up String — no
    // UnknownReferenceType diagnostic.
    let ws = analyze_cross_module_fixture("cross_module", "entry_main_param.relon");
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
    let ws = analyze_cross_module_fixture("cross_module", "strict_pkg_schema_field_mismatch.relon");
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
    let ws = analyze_cross_module_fixture("cross_module", "two_libs_same_schema_name.relon");
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
    let ws = analyze_cross_module_fixture("cross_module", "dual_alias_same_path_entry.relon");
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

/// v1.8+ cross-module type validation: a 2-segment param type whose
/// alias is valid but whose tail isn't in the alias's exported
/// schemas. Pre-fix the analyzer accepted any `pkg.Wrong` silently
/// (`subsumes_with_imports` conservative-passed and
/// `unknown_type_diagnostic` only checked single-segment paths).
#[test]
fn fixture_cross_module_pkg_unknown_schema_in_main_param() {
    let ws = analyze_cross_module_fixture("cross_module", "pkg_unknown_schema_in_main_param.relon");
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
