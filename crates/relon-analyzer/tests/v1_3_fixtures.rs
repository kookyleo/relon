//! Integration tests over `tests/fixtures/`.
//!
//! Each fixture's leading comment declares the expected outcome. We
//! parse + analyze the file (or run the workspace pass for multi-file
//! propagation cases) and assert on the diagnostic shape.

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

// ====== strict_basic ======

#[test]
fn fixture_strict_enables_bit() {
    let tree = analyze_fixture("strict_basic/strict_enables_bit.relon");
    assert!(tree.strict_mode);
}

#[test]
fn fixture_strict_demands_spread_hint() {
    let tree = analyze_fixture("strict_basic/strict_demands_spread_hint.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_demands_dynkey_hint() {
    let tree = analyze_fixture("strict_basic/strict_demands_dynkey_hint.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert!(n >= 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_typed_spread_silent() {
    let tree = analyze_fixture("strict_basic/strict_typed_spread_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_strict_typed_dynkey_silent() {
    let tree = analyze_fixture("strict_basic/strict_typed_dynkey_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ====== typehint_spread ======

#[test]
fn fixture_typehint_spread_from_main_param() {
    let tree = analyze_fixture("typehint_spread/from_main_param.relon");
    let strict_diags = count(&tree.diagnostics, |d| {
        matches!(
            d,
            Diagnostic::MissingSpreadTypeHint { .. } | Diagnostic::UnresolvedSchema { .. }
        )
    });
    assert_eq!(strict_diags, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_from_sibling_field() {
    let tree = analyze_fixture("typehint_spread/from_sibling_field.relon");
    let strict_diags = count(&tree.diagnostics, |d| {
        matches!(
            d,
            Diagnostic::MissingSpreadTypeHint { .. } | Diagnostic::UnresolvedSchema { .. }
        )
    });
    assert_eq!(strict_diags, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_from_dict_literal() {
    let tree = analyze_fixture("typehint_spread/from_dict_literal.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_strict_missing_hint() {
    let tree = analyze_fixture("typehint_spread/strict_missing_hint.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingSpreadTypeHint { .. })
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_spread_strict_unknown_schema() {
    let tree = analyze_fixture("typehint_spread/strict_unknown_schema.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::UnresolvedSchema { name, .. } if name == "Mystery"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

// ====== typehint_dynkey ======

#[test]
fn fixture_typehint_dynkey_typed_string() {
    let tree = analyze_fixture("typehint_dynkey/typed_string_key.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_typed_int() {
    let tree = analyze_fixture("typehint_dynkey/typed_int_key.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_typed_expression() {
    let tree = analyze_fixture("typehint_dynkey/typed_expression_key.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_missing_strict() {
    let tree = analyze_fixture("typehint_dynkey/missing_hint_strict.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_typehint_dynkey_non_strict_silent() {
    let tree = analyze_fixture("typehint_dynkey/non_strict_silent.relon");
    let n = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::MissingDynamicKeyTypeHint { .. })
    });
    assert_eq!(n, 0, "{:?}", tree.diagnostics);
}

// ====== dict_generics ======

#[test]
fn fixture_dict_generics_bare_compatible() {
    let tree = analyze_fixture("dict_generics/bare_dict_still_works.relon");
    assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_string_int() {
    let tree = analyze_fixture("dict_generics/dict_string_int.relon");
    assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_string_int_mismatch() {
    let tree = analyze_fixture("dict_generics/dict_string_int_mismatch.relon");
    let n = count(
        &tree.diagnostics,
        |d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "scores.art"),
    );
    assert_eq!(n, 1, "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_nested_result() {
    let tree = analyze_fixture("dict_generics/dict_nested_result.relon");
    assert!(!tree.has_errors(), "{:?}", tree.diagnostics);
}

#[test]
fn fixture_dict_generics_int_list_string() {
    let tree = analyze_fixture("dict_generics/dict_int_list_string.relon");
    // Even if `Int` keys aren't structurally validated, the type slot
    // should parse and not blow up.
    let stt = count(&tree.diagnostics, |d| {
        matches!(d, Diagnostic::StaticTypeMismatch { .. })
    });
    assert_eq!(stt, 0, "{:?}", tree.diagnostics);
}

// ====== duplicate_field ======

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

// ====== strict_propagation (multi-module) ======

/// Disk-backed loader scoped at a fixture subdirectory. Maps relative
/// `#import "./X.relon"` paths to the file contents.
struct DiskLoader {
    root: PathBuf,
    canonical: HashMap<String, String>,
}

impl DiskLoader {
    fn new(rel_dir: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
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

#[test]
fn fixture_strict_propagation_one_hop() {
    let mut loader = DiskLoader::new("strict_propagation");
    let entry_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/strict_propagation/entry.relon");
    let src = std::fs::read_to_string(&entry_path).unwrap();
    let ws = analyze_entry(
        entry_path.to_string_lossy().to_string(),
        &src,
        entry_path.parent().unwrap().to_path_buf(),
        &mut loader,
    );
    assert!(ws.strict_mode);
    for (id, tree) in &ws.modules {
        assert!(
            tree.strict_mode,
            "module {id} should be strict-tagged: {:?}",
            tree.diagnostics
        );
    }
    // The lib's silent-fallback spread should be reported.
    let total_spread_diags: usize = ws
        .modules
        .values()
        .flat_map(|t| t.diagnostics.iter())
        .filter(|d| matches!(d, Diagnostic::MissingSpreadTypeHint { .. }))
        .count();
    assert!(total_spread_diags >= 1, "expected lib's spread diag");
}

#[test]
fn fixture_strict_propagation_two_hops() {
    let mut loader = DiskLoader::new("strict_propagation");
    let entry_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/strict_propagation/chain_entry.relon");
    let src = std::fs::read_to_string(&entry_path).unwrap();
    let ws = analyze_entry(
        entry_path.to_string_lossy().to_string(),
        &src,
        entry_path.parent().unwrap().to_path_buf(),
        &mut loader,
    );
    assert!(ws.strict_mode);
    assert_eq!(ws.modules.len(), 3, "entry + mid + leaf");
    for (id, tree) in &ws.modules {
        assert!(tree.strict_mode, "{id}");
    }
}

#[test]
fn fixture_strict_propagation_diamond() {
    let mut loader = DiskLoader::new("strict_propagation");
    let entry_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/strict_propagation/diamond_entry.relon");
    let src = std::fs::read_to_string(&entry_path).unwrap();
    let ws = analyze_entry(
        entry_path.to_string_lossy().to_string(),
        &src,
        entry_path.parent().unwrap().to_path_buf(),
        &mut loader,
    );
    assert!(ws.strict_mode);
    assert_eq!(ws.modules.len(), 4, "entry + b + c + d");
    for (id, tree) in &ws.modules {
        assert!(tree.strict_mode, "{id}");
    }
}
