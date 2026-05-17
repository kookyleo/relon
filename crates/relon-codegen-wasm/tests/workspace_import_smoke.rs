//! Phase 10-b smoke tests for the wasm-AOT workspace constructor.
//!
//! Covers the multi-file `#import "./util.relon"` path that
//! `WasmAotEvaluator::from_workspace` opened up:
//!
//! * `roundtrip_imported_schema` -- entry's `#main(User u)` resolves
//!   `User` from an imported file, runs to a numeric result, and the
//!   tree-walker produces the same value off the same workspace.
//! * `duplicate_schema_across_files_rejected` -- two reachable
//!   modules declaring `User` with different shapes surface
//!   `DuplicateSchemaAcrossFiles`.
//! * `cyclic_import_rejected` -- two files importing each other are
//!   caught by the analyzer before the IR pass runs, so
//!   `from_workspace` lifts the workspace error path uniformly.
//! * `multiple_main_directives_rejected` -- an imported library that
//!   also carries `#main` fails the IR-level guard.

use relon_analyzer::workspace::{
    analyze_entry, LoadError, LoadedModule, ModuleLoader, WorkspaceTree,
};
use relon_codegen_wasm::{BuildError, WasmAotEvaluator};
use relon_eval_api::{Evaluator, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// In-memory module loader keyed by the raw import path. Mirrors the
/// `MapLoader` the analyzer's own workspace tests use so multi-file
/// fixtures stay fully hermetic.
struct MapLoader {
    files: HashMap<String, (String, String)>,
}

impl MapLoader {
    fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    /// Map a `raw` path (the literal a `#import ... from "raw"`
    /// directive would carry) to its canonical id + source text.
    fn add(&mut self, raw: &str, canonical: &str, source: &str) -> &mut Self {
        self.files
            .insert(raw.to_string(), (canonical.to_string(), source.to_string()));
        self
    }
}

impl ModuleLoader for MapLoader {
    fn load(&mut self, path: &str, _current_dir: &Path) -> Result<LoadedModule, LoadError> {
        match self.files.get(path) {
            Some((canon, source)) => Ok(LoadedModule {
                canonical_id: canon.clone(),
                source: source.clone(),
                current_dir: PathBuf::from("."),
            }),
            None => Err(LoadError::NotFound),
        }
    }
}

/// Filesystem-backed loader used by the parity-roundtrip test. The
/// in-memory `MapLoader` is enough for the IR-level guards (duplicate
/// schema / extra `#main`), but the tree-walker side of the parity
/// arm runs through `Context`'s lazy module cache, which only knows
/// how to read source off disk. Routing the analyzer pass through the
/// same filesystem here keeps the two backends pointing at the
/// identical bytes.
struct FsLoader;

impl ModuleLoader for FsLoader {
    fn load(&mut self, path: &str, current_dir: &Path) -> Result<LoadedModule, LoadError> {
        let candidate = current_dir.join(path);
        let canonical = std::fs::canonicalize(&candidate)
            .map_err(|e| LoadError::Other(format!("{}: {e}", candidate.display())))?;
        let source = std::fs::read_to_string(&canonical)
            .map_err(|e| LoadError::Other(format!("{}: {e}", canonical.display())))?;
        let dir = canonical
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(LoadedModule {
            canonical_id: canonical.to_string_lossy().to_string(),
            source,
            current_dir: dir,
        })
    }
}

fn build_workspace(entry_id: &str, entry_src: &str, loader: &mut MapLoader) -> WorkspaceTree {
    analyze_entry(entry_id.to_string(), entry_src, PathBuf::from("."), loader)
}

#[test]
fn roundtrip_imported_schema() {
    // util.relon hosts the schema; main.relon names it in `#main` and
    // walks `u.age` against the imported declaration. The wasm-AOT
    // backend without Phase 10-b had no way to see `User` here --
    // `lower_workspace_single` only ever consulted the entry tree.
    //
    // We stage the two files in a per-test tmpdir so the tree-walker
    // parity arm (which still opens files lazily via its module cache)
    // sees the same content the analyzer's workspace pass loaded.
    let dir = std::env::temp_dir().join(format!(
        "relon-wasm-phase10b-roundtrip-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let util_path = dir.join("util.relon");
    let main_path = dir.join("main.relon");
    let util_src = "#schema User { Int age: * }\n{}\n";
    let main_src = "#import * from \"./util.relon\"\n#main(User u) -> Int\nu.age * 2\n";
    std::fs::write(&util_path, util_src).unwrap();
    std::fs::write(&main_path, main_src).unwrap();

    let canonical_main = std::fs::canonicalize(&main_path).unwrap();
    let entry_id = canonical_main.to_string_lossy().to_string();
    let entry_dir = canonical_main.parent().unwrap().to_path_buf();

    let mut loader = FsLoader;
    let ws = analyze_entry(entry_id.clone(), main_src, entry_dir.clone(), &mut loader);
    assert!(
        !ws.has_errors(),
        "workspace errors: {:?}",
        ws.workspace_diagnostics
    );

    let aot = WasmAotEvaluator::from_workspace(&ws, &entry_id).expect("from_workspace");

    let mut args = HashMap::new();
    args.insert(
        "u".to_string(),
        Value::dict([("age".to_string(), Value::Int(21))].into_iter().collect()),
    );

    let aot_out = aot.run_main(args.clone()).expect("aot run_main");
    assert_eq!(aot_out, Value::Int(42));

    // Parity arm. Tree-walker opens transitive `#import`s through its
    // own module cache, which calls back into the trusted filesystem
    // resolver; that's why the fixture lives on disk instead of in
    // `MapLoader`.
    use relon_eval_api::{Capabilities, Context};
    use relon_evaluator::module::FilesystemModuleResolver;
    use relon_evaluator::{Scope, TreeWalkEvaluator};
    let entry_node = ws
        .nodes
        .get(&entry_id)
        .map(|arc| (**arc).clone())
        .expect("entry node in workspace");
    let ws_arc = Arc::new(ws);
    let mut ctx = Context::sandboxed()
        .with_root(entry_node)
        .with_workspace(Arc::clone(&ws_arc));
    ctx.capabilities = Capabilities::all_granted();
    ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let ctx = Arc::new(ctx);
    let _guard = ctx.enter_loading_module(entry_id.clone());
    let eval = TreeWalkEvaluator::new(Arc::clone(&ctx));
    let scope = Arc::new(Scope {
        current_dir: entry_dir.to_string_lossy().to_string(),
        cache_namespace: entry_id.clone(),
        ..Scope::default()
    });
    let walker_out = TreeWalkEvaluator::run_main(&eval, &scope, args).expect("tree-walk run_main");

    assert_eq!(aot_out, walker_out);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn duplicate_schema_across_files_rejected() {
    // Two reachable modules declare `User` with different shapes.
    // The analyzer's `cross_module_schema_collision` only fires for
    // spread imports; alias-form imports keep both definitions live
    // in their respective trees. wasm-AOT cannot pick between them
    // without breaking canonical-hash determinism, so the IR layer
    // surfaces `DuplicateSchemaAcrossFiles`.
    //
    // The entry imports util_a (which declares `User { Int age }`)
    // and util_b (which declares `User { String name }`) under
    // distinct aliases so analyzer-side collision detection stays
    // silent and the IR-level guard is the one that fires.
    let util_a = "#schema User { Int age: * }\n{}\n";
    let util_b = "#schema User { String name: * }\n{}\n";
    let main_src = "#import a from \"util_a.relon\"\n\
        #import b from \"util_b.relon\"\n\
        #main(Int x) -> Int\n\
        x * 2\n";

    let mut loader = MapLoader::new();
    loader.add("util_a.relon", "util_a.relon", util_a);
    loader.add("util_b.relon", "util_b.relon", util_b);

    let ws = build_workspace("main.relon", main_src, &mut loader);
    assert!(
        !ws.has_errors(),
        "expected the analyzer to accept aliased imports; got {:?}",
        ws.workspace_diagnostics
    );

    let err = match WasmAotEvaluator::from_workspace(&ws, "main.relon") {
        Ok(_) => panic!("conflicting schemas must be rejected"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        matches!(err, BuildError::LoweringError(_)),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("User"),
        "message should name the schema: {msg}"
    );
}

#[test]
fn cyclic_import_rejected() {
    // a.relon and b.relon import each other. The analyzer's
    // workspace pass detects the cycle and stamps the workspace
    // with `WorkspaceDiagnostic::CircularImport`; `has_errors`
    // flips, so the host should refuse to hand the workspace to
    // `from_workspace`. We surface that as a defensive assertion
    // here so the contract stays visible -- the IR pass itself
    // would not have to recover from a cycle that never reached it.
    let a_src = "#import b from \"b.relon\"\n#main(Int x) -> Int\nx + 1\n";
    let b_src = "#import a from \"a.relon\"\n{}\n";

    let mut loader = MapLoader::new();
    loader.add("a.relon", "a.relon", a_src);
    loader.add("b.relon", "b.relon", b_src);

    let ws = build_workspace("a.relon", a_src, &mut loader);
    assert!(ws.has_errors(), "cycle should surface as workspace error");
    let saw_cycle = ws.workspace_diagnostics.iter().any(|d| {
        matches!(
            d,
            relon_analyzer::WorkspaceDiagnostic::CircularImport { .. }
        )
    });
    assert!(
        saw_cycle,
        "expected CircularImport in workspace diagnostics; got {:?}",
        ws.workspace_diagnostics
    );
}

#[test]
fn multiple_main_directives_rejected() {
    // Only the entry file may declare `#main`. If an imported
    // library also has one, the IR pass guards the build with
    // `MultipleMainDirectives` so the user has to pick which file
    // is meant to be the entry.
    let lib_src = "#main(Int y) -> Int\ny + 1\n";
    let main_src = "#import lib from \"lib.relon\"\n#main(Int x) -> Int\nx * 2\n";

    let mut loader = MapLoader::new();
    loader.add("lib.relon", "lib.relon", lib_src);

    let ws = build_workspace("main.relon", main_src, &mut loader);
    assert!(
        !ws.has_errors(),
        "analyzer should accept two `#main`s -- IR pass is the gate"
    );

    let err = match WasmAotEvaluator::from_workspace(&ws, "main.relon") {
        Ok(_) => panic!("workspace with two #main directives must be rejected"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        matches!(err, BuildError::LoweringError(_)),
        "unexpected error: {msg}"
    );
    assert!(
        msg.contains("multiple `#main`"),
        "message should mention the multi-main guard: {msg}"
    );
}

#[test]
fn cross_file_schema_hash_stable() {
    // Sanity check on the canonical-hash contract: the same `User`
    // schema declared in an imported file vs a single-file program
    // must produce identical `#main` schemas (and therefore the same
    // `relon.abi` hash). This is the deterministic-cache anchor the
    // wasm-AOT pipeline relies on when an entry that used to be
    // self-contained is refactored to pull `User` from a sibling
    // module without renaming anything.
    use relon_eval_api::schema_canonical::schema_hash;

    let single_src = "#schema User { Int age: * }\n#main(User u) -> Int\nu.age\n";
    let single_aot = WasmAotEvaluator::from_source(single_src).expect("single-file compile");
    let single_hash = schema_hash(single_aot.main_schema());

    let util_src = "#schema User { Int age: * }\n{}\n";
    let main_src = "#import * from \"util.relon\"\n#main(User u) -> Int\nu.age\n";
    let mut loader = MapLoader::new();
    loader.add("util.relon", "util.relon", util_src);
    let ws = build_workspace("main.relon", main_src, &mut loader);
    assert!(!ws.has_errors());
    let multi_aot = WasmAotEvaluator::from_workspace(&ws, "main.relon").expect("workspace compile");
    let multi_hash = schema_hash(multi_aot.main_schema());

    assert_eq!(
        single_hash, multi_hash,
        "moving `User` to an imported file should not perturb the canonical hash"
    );
}
