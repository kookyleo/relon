//! Issue #1, step 1 — the public `#relaxed` stdlib wrappers
//! (`list.sum`, `list.first`, `list.map`, ...) must flow their return
//! type through the two-segment `alias.method(...)` call path instead of
//! collapsing to `Any`.
//!
//! These tests build a small workspace whose only import is a `std/*`
//! module, resolved to the *real* evaluator source (no drift), and
//! assert the caller's typed binding is now checked precisely:
//! `Int total: list.sum([1,2,3])` type-checks, while
//! `String total: list.sum([1,2,3])` is now rejected — the exact
//! behavior that `Any` used to mask.

use relon_analyzer::{analyze_entry, workspace::LoadError, workspace::LoadedModule};
use relon_analyzer::{Diagnostic, WorkspaceTree};
use std::path::{Path, PathBuf};

/// Loader that resolves `std/<name>` imports to the real stdlib source
/// shipped in `relon-evaluator/src/std_relon/<name>.relon`. Reading the
/// on-disk source (rather than an inline copy) keeps the test honest
/// against future edits to the wrappers.
struct StdLoader;

impl relon_analyzer::workspace::ModuleLoader for StdLoader {
    fn load(&mut self, path: &str, _current_dir: &Path) -> Result<LoadedModule, LoadError> {
        let name = path.strip_prefix("std/").ok_or(LoadError::NotFound)?;
        let src_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../relon-evaluator/src/std_relon")
            .join(format!("{name}.relon"));
        let source = std::fs::read_to_string(&src_path).map_err(|_| LoadError::NotFound)?;
        Ok(LoadedModule {
            canonical_id: format!("std/{name}"),
            source,
            current_dir: src_path.parent().unwrap().to_path_buf(),
        })
    }
}

/// Analyze `entry_src` as a workspace whose `std/*` imports resolve to
/// the real stdlib sources. Returns the entry module's diagnostics.
fn analyze_entry_src(entry_src: &str) -> WorkspaceTree {
    let mut loader = StdLoader;
    analyze_entry(
        "entry.relon".to_string(),
        entry_src,
        PathBuf::from("."),
        &mut loader,
    )
}

fn type_mismatches(ws: &WorkspaceTree) -> usize {
    ws.entry_tree()
        .expect("entry module analyzed")
        .diagnostics
        .iter()
        .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
        .count()
}

// ---- list.sum: the core step-1 fix -------------------------------------

#[test]
fn list_sum_flows_int_return() {
    let ws = analyze_entry_src(
        "#import list from \"std/list\"\n{\n  Int total: list.sum([1, 2, 3])\n}\n",
    );
    assert_eq!(
        type_mismatches(&ws),
        0,
        "Int total: list.sum([1,2,3]) should type-check (sum now returns Int, not Any): {:?}",
        ws.entry_tree().unwrap().diagnostics
    );
}

#[test]
fn list_sum_string_slot_now_rejected() {
    // The crux of step 1: before the wrapper-signature overlay, `sum`
    // returned `Any`, which subsumed `String`, so this silently passed.
    // Now `sum([1,2,3])` is `Int` and the `String` slot is a real
    // mismatch.
    let ws = analyze_entry_src(
        "#import list from \"std/list\"\n{\n  String total: list.sum([1, 2, 3])\n}\n",
    );
    assert!(
        type_mismatches(&ws) >= 1,
        "String total: list.sum([1,2,3]) must now be rejected (Int vs String): {:?}",
        ws.entry_tree().unwrap().diagnostics
    );
}

#[test]
fn list_sum_float_flows_float_return() {
    let ws = analyze_entry_src(
        "#import list from \"std/list\"\n{\n  Float total: list.sum([1.0, 2.0])\n}\n",
    );
    assert_eq!(
        type_mismatches(&ws),
        0,
        "sum of a Float list should flow Float: {:?}",
        ws.entry_tree().unwrap().diagnostics
    );
}

// ---- list.first: element type flows ------------------------------------

#[test]
fn list_first_flows_element_type() {
    let ok =
        analyze_entry_src("#import list from \"std/list\"\n{\n  Int x: list.first([1, 2, 3])\n}\n");
    assert_eq!(
        type_mismatches(&ok),
        0,
        "Int x: list.first([1,2,3]) should type-check: {:?}",
        ok.entry_tree().unwrap().diagnostics
    );

    let bad = analyze_entry_src(
        "#import list from \"std/list\"\n{\n  String x: list.first([1, 2, 3])\n}\n",
    );
    assert!(
        type_mismatches(&bad) >= 1,
        "String x: list.first([1,2,3]) must now be rejected: {:?}",
        bad.entry_tree().unwrap().diagnostics
    );
}

// ---- list.map: element type flows through the closure ------------------

#[test]
fn list_map_flows_list_of_closure_return() {
    let ok = analyze_entry_src(
        "#import list from \"std/list\"\n{\n  List<Int> xs: list.map([1, 2, 3], (x) => x + 1)\n}\n",
    );
    assert_eq!(
        type_mismatches(&ok),
        0,
        "List<Int> from map((x)=>x+1) should type-check: {:?}",
        ok.entry_tree().unwrap().diagnostics
    );

    let bad = analyze_entry_src(
        "#import list from \"std/list\"\n{\n  List<String> xs: list.map([1, 2, 3], (x) => x + 1)\n}\n",
    );
    assert!(
        type_mismatches(&bad) >= 1,
        "List<String> from an Int-producing map must now be rejected: {:?}",
        bad.entry_tree().unwrap().diagnostics
    );
}
