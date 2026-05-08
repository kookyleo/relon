//! Workspace-build pass: turn an entry source into a `WorkspaceTree`.
//!
//! Split out of `workspace.rs` to keep the public type definitions
//! (which LSP and the evaluator pin against) separate from the BFS /
//! cycle-detection machinery, which is implementation-detail-heavy.

use crate::diagnostic::Diagnostic;
use crate::tree::AnalyzedTree;
use crate::workspace::{LoadError, ModuleLoader, WorkspaceDiagnostic, WorkspaceTree};
use miette::SourceSpan;
use relon_parser::{parse_document, Node, TokenRange};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

/// A module slot in the BFS queue. We carry the full canonical id of
/// the *importer* + the directive's range so cycle / not-found reports
/// can point at the actual `#import` site rather than at the imported
/// module's own root.
struct PendingImport {
    importer_id: String,
    importer_dir: PathBuf,
    /// Path written in the `#import` directive (verbatim), the value
    /// passed to `ModuleLoader::load`.
    raw_path: String,
    /// Range of the importing directive in the *importer*. The
    /// `WorkspaceDiagnostic`s that surface here are reported against
    /// the importer's source.
    range: TokenRange,
}

pub(crate) fn build<L: ModuleLoader>(
    entry_id: String,
    entry_source: &str,
    entry_current_dir: PathBuf,
    loader: &mut L,
    options: &crate::AnalyzeOptions,
) -> WorkspaceTree {
    let mut ws = WorkspaceTree::new();
    ws.entry_id = entry_id.clone();

    // Track per-module current_dir so transitive `#import "./x"` paths
    // resolve relative to the *importer* the way the evaluator does at
    // runtime. The map is keyed by canonical id.
    let mut module_dirs: HashMap<String, PathBuf> = HashMap::new();
    module_dirs.insert(entry_id.clone(), entry_current_dir.clone());

    // 1. Analyze the entry first; failure here doesn't short-circuit —
    //    we still want the workspace tree to carry the parse error so
    //    callers can render it.
    match parse_document(entry_source) {
        Ok(node) => {
            let arc_node = Arc::new(node);
            let tree = crate::analyze_with_options(&arc_node, options);
            let imports = collect_import_targets(&tree, &entry_id, &entry_current_dir);
            ws.import_graph.insert(
                entry_id.clone(),
                imports.iter().map(|p| p.raw_path.clone()).collect(),
            );
            ws.modules.insert(entry_id.clone(), Arc::new(tree));
            ws.nodes.insert(entry_id.clone(), arc_node);
            // Seed BFS queue.
            let mut queue: VecDeque<PendingImport> = imports.into_iter().collect();
            // BFS: for each pending import, resolve it through the
            // loader, parse + analyze, then enqueue *its* imports.
            // Modules already in `ws.modules` are skipped (their
            // canonical id is the dedup key — the loader is what maps
            // raw paths to canonical ids, so we have to call it to know
            // whether a module is already loaded).
            //
            // `seen_raw` short-circuits the common case where the same
            // file is `#import`ed twice from the same importer with the
            // same relative path: we skip the loader call entirely.
            // Different relative paths to the same canonical id still
            // dedup downstream via `ws.modules.contains_key`.
            let mut seen_raw: HashSet<(String, String)> = HashSet::new();
            while let Some(item) = queue.pop_front() {
                let key = (item.importer_id.clone(), item.raw_path.clone());
                if !seen_raw.insert(key) {
                    continue;
                }
                process_import(item, loader, &mut ws, &mut queue, &mut module_dirs, options);
            }
        }
        Err(parse_err) => {
            // Even the entry didn't parse; record a synthetic
            // workspace-level diagnostic so `has_errors` flips and the
            // host sees something coherent. The entry never lands in
            // `modules` because there's no `AnalyzedTree` to attach.
            ws.workspace_diagnostics
                .push(WorkspaceDiagnostic::ModuleParseError {
                    path: entry_id.clone(),
                    message: parse_err.to_string(),
                    // No directive range to anchor on (the entry isn't
                    // imported by anyone), so we use a zero-length span at
                    // offset 0 of the entry source.
                    range: SourceSpan::from((0usize, 0usize)),
                });
        }
    }

    // 2. Cycle detection over the resolved import graph. We run a
    //    classic three-color DFS and emit one CircularImport diagnostic
    //    per back-edge.
    detect_cycles(&mut ws);

    // 3. Cross-module schema collisions surfaced via spread imports.
    //    Done after the BFS so every reachable module is in `modules`.
    detect_cross_module_schema_collisions(&mut ws);

    // 4. Stage 2.1: with all reachable modules analyzed, build a
    //    workspace-wide import index keyed by canonical id and walk
    //    each module's diagnostics to *remove* `UnknownTypeName`
    //    warnings whose head is actually visible through a cross-module
    //    `#import`. This is a pure post-pass over `modules` — it never
    //    re-runs analyzer state, only filters already-emitted
    //    diagnostics. The single-file `analyze` call has no idea what
    //    modules `#import * from "..."` brings in, so the false-positive
    //    correction has to happen here.
    re_check_unknown_types(&mut ws);

    // 5. Stage 4: cross-module capability reachability. Runs after the
    //    import index is settled because the walker needs every
    //    reachable module's `node_index` populated. Diagnostics land
    //    on the entry tree.
    crate::capability_check::run(&mut ws);

    ws
}

fn process_import<L: ModuleLoader>(
    item: PendingImport,
    loader: &mut L,
    ws: &mut WorkspaceTree,
    queue: &mut VecDeque<PendingImport>,
    module_dirs: &mut HashMap<String, PathBuf>,
    options: &crate::AnalyzeOptions,
) {
    let span = SourceSpan::from(item.range);
    match loader.load(&item.raw_path, &item.importer_dir) {
        Ok(loaded) => {
            // Wire `importer -> loaded.canonical_id` into the import
            // graph (replacing the raw_path edge that was provisionally
            // recorded earlier so cycle detection has canonical ids).
            if let Some(edges) = ws.import_graph.get_mut(&item.importer_id) {
                for edge in edges.iter_mut() {
                    if edge == &item.raw_path {
                        *edge = loaded.canonical_id.clone();
                        break;
                    }
                }
            }
            if ws.modules.contains_key(&loaded.canonical_id) {
                // Already analyzed; don't re-enqueue but the graph
                // edge is now resolved, which is what cycle detection
                // needs.
                return;
            }
            module_dirs.insert(loaded.canonical_id.clone(), loaded.current_dir.clone());
            match parse_document(&loaded.source) {
                Ok(node) => {
                    let arc_node = Arc::new(node);
                    let tree = crate::analyze_with_options(&arc_node, options);
                    let imports =
                        collect_import_targets(&tree, &loaded.canonical_id, &loaded.current_dir);
                    ws.import_graph.insert(
                        loaded.canonical_id.clone(),
                        imports.iter().map(|p| p.raw_path.clone()).collect(),
                    );
                    ws.modules
                        .insert(loaded.canonical_id.clone(), Arc::new(tree));
                    ws.nodes.insert(loaded.canonical_id.clone(), arc_node);
                    for next in imports {
                        queue.push_back(next);
                    }
                }
                Err(parse_err) => {
                    ws.workspace_diagnostics
                        .push(WorkspaceDiagnostic::ModuleParseError {
                            path: loaded.canonical_id.clone(),
                            message: parse_err.to_string(),
                            range: span,
                        });
                    // Insert an empty AnalyzedTree shell + import-graph
                    // entry so cycle detection / collision analysis
                    // doesn't trip over a "ghost" canonical id.
                    ws.modules
                        .insert(loaded.canonical_id.clone(), Arc::new(AnalyzedTree::new()));
                    ws.import_graph
                        .insert(loaded.canonical_id.clone(), Vec::new());
                }
            }
        }
        Err(LoadError::NotFound) => {
            ws.workspace_diagnostics
                .push(WorkspaceDiagnostic::ModuleNotFound {
                    path: item.raw_path.clone(),
                    range: span,
                });
        }
        Err(LoadError::AccessDenied(reason)) => {
            // Treated as ModuleNotFound for the user; the reason is
            // surfaced through the help text via formatting.
            ws.workspace_diagnostics
                .push(WorkspaceDiagnostic::ModuleNotFound {
                    path: format!("{} ({reason})", item.raw_path),
                    range: span,
                });
        }
        Err(LoadError::Other(message)) => {
            ws.workspace_diagnostics
                .push(WorkspaceDiagnostic::ModuleParseError {
                    path: item.raw_path.clone(),
                    message,
                    range: span,
                });
        }
    }
}

/// Pull `#import` directives out of an analyzed tree, packaged into
/// `PendingImport` entries the BFS queue understands. Imports without a
/// static path (`path == None`) are skipped — they're a parser-side
/// future syntax that the runtime evaluates dynamically; the workspace
/// pass intentionally has no opinion on them.
fn collect_import_targets(
    tree: &AnalyzedTree,
    importer_id: &str,
    importer_dir: &std::path::Path,
) -> Vec<PendingImport> {
    let mut out = Vec::new();
    for imp in &tree.imports {
        let Some(path) = imp.path.clone() else {
            continue;
        };
        out.push(PendingImport {
            importer_id: importer_id.to_string(),
            importer_dir: importer_dir.to_path_buf(),
            raw_path: path,
            range: imp.range,
        });
    }
    out
}

/// Three-color DFS: white (unvisited), gray (on stack), black (done).
/// A back-edge to a gray vertex closes a cycle. We emit one
/// `CircularImport` per detected back-edge; the chain is the gray-stack
/// slice from the target of the back-edge through the current vertex,
/// followed by the back-edge target itself (so the chain ends and
/// starts on the same canonical id).
fn detect_cycles(ws: &mut WorkspaceTree) {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: HashMap<String, Color> = ws
        .import_graph
        .keys()
        .map(|k| (k.clone(), Color::White))
        .collect();
    // Nodes that appear only as edge targets (e.g. modules that failed
    // to parse and got an empty edge list) still need to participate.
    for edges in ws.import_graph.values() {
        for v in edges {
            color.entry(v.clone()).or_insert(Color::White);
        }
    }
    let mut stack: Vec<String> = Vec::new();
    let mut emitted: HashSet<Vec<String>> = HashSet::new();

    let nodes: Vec<String> = color.keys().cloned().collect();
    for start in nodes {
        if color.get(&start).copied().unwrap_or(Color::Black) != Color::White {
            continue;
        }
        dfs(&start, ws, &mut color, &mut stack, &mut emitted);
    }

    fn dfs(
        node: &str,
        ws: &mut WorkspaceTree,
        color: &mut HashMap<String, Color>,
        stack: &mut Vec<String>,
        emitted: &mut HashSet<Vec<String>>,
    ) {
        color.insert(node.to_string(), Color::Gray);
        stack.push(node.to_string());

        let edges: Vec<String> = ws.import_graph.get(node).cloned().unwrap_or_default();
        for next in edges {
            match color.get(&next).copied().unwrap_or(Color::White) {
                Color::White => dfs(&next, ws, color, stack, emitted),
                Color::Gray => {
                    // Back-edge: cycle closes here.
                    if let Some(start_idx) = stack.iter().position(|s| s == &next) {
                        let mut chain: Vec<String> = stack[start_idx..].to_vec();
                        chain.push(next.clone());
                        if emitted.insert(chain.clone()) {
                            // Locate the importing directive for the
                            // edge `node -> next` so the diagnostic
                            // range points at the actual `#import`.
                            let range = locate_import_range(ws, node, &next).unwrap_or_default();
                            ws.workspace_diagnostics
                                .push(WorkspaceDiagnostic::CircularImport {
                                    chain,
                                    range: SourceSpan::from(range),
                                });
                        }
                    }
                }
                Color::Black => {}
            }
        }
        color.insert(node.to_string(), Color::Black);
        stack.pop();
    }
}

/// Find the token range of the directive in `importer` that resolved
/// to `target`. We re-walk the importer's `imports` list and match by
/// canonical id (post-resolution) — paths are matched verbatim against
/// import_graph entries, so the equality check is straightforward as
/// long as the BFS rewrote the graph entry to the canonical id.
fn locate_import_range(ws: &WorkspaceTree, importer: &str, target: &str) -> Option<TokenRange> {
    let tree = ws.modules.get(importer)?;
    // The BFS rewrote `import_graph[importer]` so each entry is a
    // canonical id when resolution succeeded; but the `tree.imports`
    // list still carries raw paths. Walk both in lockstep to find the
    // raw_path that mapped to `target`.
    let edges = ws.import_graph.get(importer)?;
    for (idx, edge) in edges.iter().enumerate() {
        if edge == target {
            return tree.imports.get(idx).map(|imp| imp.range);
        }
    }
    None
}

/// Names from cross-module `#import` directives that *one specific
/// importer* can see, organized by binding kind.
///
/// Built per-module by [`build_import_index`] using the workspace's
/// module graph + already-analyzed `root_schemas` lists. Stage 2 uses
/// it for two purposes: (a) re-checking `UnknownTypeName` (this file)
/// and (b) `pkg.Type` multi-segment subsumption (`infer.rs`).
#[derive(Debug, Default, Clone)]
pub(crate) struct WorkspaceImportIndex {
    /// `alias → set of root-level schema names exposed by the imported
    /// module`. Drives `pkg.Type` resolution.
    pub aliased: HashMap<String, HashSet<String>>,
    /// Names brought in via `#import * from "..."` — the union of every
    /// spread target's root-level schema names. Drives bare `Type`
    /// resolution for the importer.
    pub spread: HashSet<String>,
    /// Names brought in via `#import { a, b as c } from "..."`. Map
    /// keys are the *local* names (alias when present, else upstream).
    /// Values are the upstream schema names — currently unused by
    /// downstream passes but kept for future "go to definition" tooling.
    pub destructured: HashMap<String, String>,
}

impl WorkspaceImportIndex {
    /// True when `name` is visible to the importer through some
    /// cross-module form (spread or destructure). Alias-form imports
    /// don't expose schema names directly — they go through `pkg.Type`.
    pub(crate) fn knows(&self, name: &str) -> bool {
        self.spread.contains(name) || self.destructured.contains_key(name)
    }
}

/// Build a [`WorkspaceImportIndex`] for one module, using its already-analyzed
/// `tree.imports` plus the workspace's module graph + per-module
/// `root_schemas` lists. The function is read-only over `ws` and safe to
/// call after `build()`'s BFS has populated every reachable module.
pub(crate) fn build_import_index(ws: &WorkspaceTree, importer_id: &str) -> WorkspaceImportIndex {
    let mut index = WorkspaceImportIndex::default();
    let Some(tree) = ws.modules.get(importer_id) else {
        return index;
    };
    let edges = ws
        .import_graph
        .get(importer_id)
        .cloned()
        .unwrap_or_default();
    // `tree.imports` and `edges` are walked in lockstep — the workspace
    // build pass rewrote each `import_graph[importer]` entry to the
    // canonical id of the resolved module, in source order, so the i-th
    // import's resolved target is `edges[i]`. Imports that failed to
    // resolve still occupy the slot (with the raw path), but their
    // module won't be in `ws.modules`.
    for (idx, imp) in tree.imports.iter().enumerate() {
        let Some(target_id) = edges.get(idx) else {
            continue;
        };
        let Some(target_tree) = ws.modules.get(target_id) else {
            continue;
        };
        let exported_names: HashSet<String> = target_tree
            .root_schemas
            .iter()
            .map(|d| d.name.clone())
            .collect();
        if let Some(alias) = &imp.alias {
            index
                .aliased
                .entry(alias.clone())
                .or_default()
                .extend(exported_names.iter().cloned());
            continue;
        }
        if imp.spread {
            index.spread.extend(exported_names);
            continue;
        }
        if !imp.destructure.is_empty() {
            for (upstream, alias) in &imp.destructure {
                // Only expose names actually exported by the target.
                if !exported_names.contains(upstream) {
                    continue;
                }
                let local = alias.clone().unwrap_or_else(|| upstream.clone());
                index.destructured.insert(local, upstream.clone());
            }
        }
    }
    index
}

/// Stage 2.1 post-pass: walk every module's diagnostics and drop
/// `UnknownTypeName` entries whose head is now resolvable through the
/// workspace-level import index. Handles the cross-module variants of
/// the Stage 1.8 check (`#main(LibType x)` after `#import * from "lib"`,
/// `#main(lib.Type x)` after `#import lib from "lib"`, ...).
fn re_check_unknown_types(ws: &mut WorkspaceTree) {
    // Pre-build every module's index so the inner loop doesn't
    // recompute on each diagnostic. Borrow `ws` immutably here, then
    // drop the borrow before mutating per-module trees below.
    let mut indexes: HashMap<String, WorkspaceImportIndex> = HashMap::new();
    let module_ids: Vec<String> = ws.modules.keys().cloned().collect();
    for id in &module_ids {
        indexes.insert(id.clone(), build_import_index(ws, id));
    }
    for id in &module_ids {
        let Some(index) = indexes.get(id) else {
            continue;
        };
        let Some(arc_tree) = ws.modules.get_mut(id) else {
            continue;
        };
        // `Arc::get_mut` works because `build()` is the only caller and
        // we still hold the unique reference. If a future caller starts
        // sharing the Arc earlier, swap to `Arc::make_mut` (clone-on-
        // write); the compiler will surface the missing `Clone` impl.
        let Some(tree) = Arc::get_mut(arc_tree) else {
            continue;
        };
        tree.diagnostics.retain(|d| !is_now_known_type(d, index));
    }
}

/// Decide whether `d` is an `UnknownTypeName` whose head is visible
/// through `index`. Anything else (any other diagnostic, or an
/// `UnknownTypeName` head that's still unknown) is kept as-is.
fn is_now_known_type(d: &Diagnostic, index: &WorkspaceImportIndex) -> bool {
    let Diagnostic::UnknownTypeName { name, .. } = d else {
        return false;
    };
    index.knows(name)
}

/// Detect schemas with the same name that surface from two different
/// spread imports of the entry file. Only top-level schemas (entries
/// in `tree.root_schemas`) participate — a schema that's nested inside
/// a dict isn't reachable through `#import *`.
fn detect_cross_module_schema_collisions(ws: &mut WorkspaceTree) {
    let Some(entry_tree) = ws.modules.get(&ws.entry_id).cloned() else {
        return;
    };
    let entry_node = ws.nodes.get(&ws.entry_id).cloned();

    // Build set of spread-imported modules (canonical ids the entry
    // imports with `#import *`).
    let mut spread_targets: Vec<(String, TokenRange)> = Vec::new();
    let edges = ws
        .import_graph
        .get(&ws.entry_id)
        .cloned()
        .unwrap_or_default();
    for (idx, imp) in entry_tree.imports.iter().enumerate() {
        if !imp.spread {
            continue;
        }
        let Some(target) = edges.get(idx) else {
            continue;
        };
        spread_targets.push((target.clone(), imp.range));
    }

    if spread_targets.len() < 2 {
        return;
    }

    // For every spread target, list the schema names it exports
    // (top-level `#schema Name Body` decls). The first occurrence wins,
    // every subsequent one collides.
    let mut owner_of: HashMap<String, String> = HashMap::new();
    for (target_id, imp_range) in &spread_targets {
        let Some(tree) = ws.modules.get(target_id) else {
            continue;
        };
        let anchor_range = *imp_range;
        for decl in &tree.root_schemas {
            if let Some(prev) = owner_of.get(&decl.name) {
                ws.workspace_diagnostics
                    .push(WorkspaceDiagnostic::CrossModuleSchemaCollision {
                        name: decl.name.clone(),
                        first: prev.clone(),
                        second: target_id.clone(),
                        range: SourceSpan::from(anchor_range),
                    });
            } else {
                owner_of.insert(decl.name.clone(), target_id.clone());
            }
        }
    }

    // Silence unused-warning: `entry_node` is kept for future passes
    // that need a root span fallback, and `Node` is used only via
    // re-exports / function signatures.
    let _ = entry_node;
    let _ = std::marker::PhantomData::<Node>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{LoadError, LoadedModule, ModuleLoader, WorkspaceDiagnostic};
    use std::collections::HashMap;
    use std::path::Path;

    /// Test wrapper — calls the production `build` with default
    /// `AnalyzeOptions`, matching the pre-Stage-2.4 signature used by
    /// every existing test in this module.
    fn build<L: ModuleLoader>(
        entry_id: String,
        entry_source: &str,
        entry_current_dir: PathBuf,
        loader: &mut L,
    ) -> WorkspaceTree {
        super::build(
            entry_id,
            entry_source,
            entry_current_dir,
            loader,
            &crate::AnalyzeOptions::default(),
        )
    }

    /// In-memory test loader: maps raw paths to (canonical_id, source).
    /// `current_dir` is ignored — every entry is "absolute" in the
    /// loader's world view.
    struct MapLoader {
        files: HashMap<String, (String, String)>,
    }

    impl MapLoader {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
            }
        }
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

    #[test]
    fn single_file_no_imports() {
        let mut loader = MapLoader::new();
        let ws = build(
            "entry.relon".to_string(),
            "{ a: 1 }",
            PathBuf::from("."),
            &mut loader,
        );
        assert!(!ws.has_errors());
        assert_eq!(ws.modules.len(), 1);
        assert!(ws.modules.contains_key("entry.relon"));
    }

    #[test]
    fn detects_self_cycle() {
        let mut loader = MapLoader::new();
        loader.add(
            "circular.relon",
            "/abs/circular.relon",
            r#"{
                #import circular from "circular.relon",
                "self": "oops"
            }"#,
        );
        let ws = build(
            "/abs/circular.relon".to_string(),
            r#"{
                #import circular from "circular.relon",
                "self": "oops"
            }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(ws.has_errors(), "expected workspace to flag self-import");
        let cycles: Vec<_> = ws
            .workspace_diagnostics
            .iter()
            .filter(|d| matches!(d, WorkspaceDiagnostic::CircularImport { .. }))
            .collect();
        assert_eq!(cycles.len(), 1, "{:?}", ws.workspace_diagnostics);
    }

    #[test]
    fn detects_a_b_a_cycle() {
        let mut loader = MapLoader::new();
        loader
            .add(
                "./b.relon",
                "/abs/b.relon",
                r#"#import a from "./a.relon"
                { from_b: a }"#,
            )
            .add(
                "./a.relon",
                "/abs/a.relon",
                r#"#import b from "./b.relon"
                { from_a: b }"#,
            );
        let ws = build(
            "/abs/a.relon".to_string(),
            r#"#import b from "./b.relon"
            { from_a: b }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(ws.has_errors());
        assert!(
            ws.workspace_diagnostics
                .iter()
                .any(|d| matches!(d, WorkspaceDiagnostic::CircularImport { .. })),
            "{:?}",
            ws.workspace_diagnostics
        );
        // Both modules should still be in the tree (cycle detection is
        // observational, not destructive).
        assert!(ws.modules.contains_key("/abs/a.relon"));
        assert!(ws.modules.contains_key("/abs/b.relon"));
    }

    #[test]
    fn linear_chain_records_three_modules() {
        let mut loader = MapLoader::new();
        loader
            .add(
                "./b.relon",
                "/abs/b.relon",
                r#"#import c from "./c.relon"
                { val: c }"#,
            )
            .add("./c.relon", "/abs/c.relon", r#"{ leaf: 1 }"#);
        let ws = build(
            "/abs/a.relon".to_string(),
            r#"#import b from "./b.relon"
            { top: b }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(!ws.has_errors(), "{:?}", ws.workspace_diagnostics);
        assert_eq!(ws.modules.len(), 3);
    }

    #[test]
    fn missing_module_reports_not_found() {
        let mut loader = MapLoader::new();
        let ws = build(
            "/abs/a.relon".to_string(),
            r#"#import x from "./missing.relon"
            { x: 1 }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(ws.has_errors());
        assert!(
            ws.workspace_diagnostics
                .iter()
                .any(|d| matches!(d, WorkspaceDiagnostic::ModuleNotFound { .. })),
            "{:?}",
            ws.workspace_diagnostics
        );
    }

    #[test]
    fn module_with_analyze_error_propagates() {
        let mut loader = MapLoader::new();
        // B has a #schema body that isn't a dict; analyzer should flag it.
        loader.add(
            "./b.relon",
            "/abs/b.relon",
            r#"#schema X 42
            { ok: 1 }"#,
        );
        let ws = build(
            "/abs/a.relon".to_string(),
            r#"#import b from "./b.relon"
            { v: b.ok }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(ws.has_errors(), "{:?}", ws.workspace_diagnostics);
        // Error came from a per-module analyzer pass, not a workspace
        // diagnostic.
        let module_b = ws.modules.get("/abs/b.relon").unwrap();
        assert!(module_b.has_errors());
    }

    #[test]
    fn parse_error_in_imported_module_is_aggregated() {
        let mut loader = MapLoader::new();
        // B is unparseable; C is fine. Both should be visited; only B
        // surfaces a workspace_diagnostic.
        loader.add("./b.relon", "/abs/b.relon", "{ unclosed").add(
            "./c.relon",
            "/abs/c.relon",
            r#"{ leaf: 1 }"#,
        );
        let ws = build(
            "/abs/a.relon".to_string(),
            r#"#import b from "./b.relon"
            #import c from "./c.relon"
            { v: 1 }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(ws
            .workspace_diagnostics
            .iter()
            .any(|d| matches!(d, WorkspaceDiagnostic::ModuleParseError { path, .. } if path == "/abs/b.relon")));
        // C should still be analyzed.
        assert!(ws.modules.contains_key("/abs/c.relon"));
    }

    // === Stage 0.5 coverage matrix ===

    #[test]
    fn single_file_workspace_matches_single_file_analyze() {
        // Test #1: walking the workspace pass over a single file with
        // no imports must produce the same per-file analysis as the
        // direct `analyze` API. Compares the side-table sizes;
        // comparing `NodeId`s would fail because the workspace pass
        // re-parses internally and minted fresh ids.
        let src = "{ a: 1, b: \"hi\", c: [1, 2, 3] }";
        let mut loader = MapLoader::new();
        let ws = build("entry".to_string(), src, PathBuf::from("."), &mut loader);
        assert!(!ws.has_errors());
        let entry = ws.entry_tree().unwrap();
        let standalone = crate::analyze(&parse_document(src).unwrap());
        assert_eq!(entry.schemas.len(), standalone.schemas.len());
        assert_eq!(entry.references.len(), standalone.references.len());
        assert_eq!(entry.imports.len(), standalone.imports.len());
        assert_eq!(entry.root_schemas.len(), standalone.root_schemas.len());
        assert_eq!(entry.diagnostics.len(), standalone.diagnostics.len());
    }

    #[test]
    fn three_module_chain_with_zero_errors() {
        // Test #6: three reachable modules in a linear chain, no
        // workspace errors, every module recorded.
        let mut loader = MapLoader::new();
        loader
            .add(
                "./b",
                "/abs/b",
                r#"#import c from "./c"
                { mid: c.leaf }"#,
            )
            .add("./c", "/abs/c", r#"{ leaf: 7 }"#);
        let ws = build(
            "/abs/a".to_string(),
            r#"#import b from "./b"
            { top: b.mid }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(!ws.has_errors(), "{:?}", ws.workspace_diagnostics);
        assert_eq!(ws.modules.len(), 3);
        assert!(ws.modules.contains_key("/abs/a"));
        assert!(ws.modules.contains_key("/abs/b"));
        assert!(ws.modules.contains_key("/abs/c"));
    }

    #[test]
    fn spread_imports_without_collision_pass() {
        // Test #7: two `#import *` modules with disjoint top-level
        // schemas — workspace must not flag a collision.
        let mut loader = MapLoader::new();
        loader
            .add(
                "./b",
                "/abs/b",
                r#"#schema User { String name: * }
                { default_user: User { name: "x" } }"#,
            )
            .add(
                "./c",
                "/abs/c",
                r#"#schema Order { Int id: (n) => n > 0 }
                { sample: Order { id: 1 } }"#,
            );
        let ws = build(
            "/abs/a".to_string(),
            r#"#import * from "./b"
            #import * from "./c"
            { combined: 1 }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        let collisions: Vec<_> = ws
            .workspace_diagnostics
            .iter()
            .filter(|d| matches!(d, WorkspaceDiagnostic::CrossModuleSchemaCollision { .. }))
            .collect();
        assert!(
            collisions.is_empty(),
            "unexpected collisions: {collisions:?}"
        );
    }

    #[test]
    fn spread_imports_collision_is_reported() {
        // Test #8: both spread-imported modules declare a top-level
        // `User` schema; workspace must report the collision.
        let mut loader = MapLoader::new();
        loader
            .add(
                "./b",
                "/abs/b",
                r#"#schema User { String name: * }
                { x: 1 }"#,
            )
            .add(
                "./c",
                "/abs/c",
                r#"#schema User { Int id: (n) => n > 0 }
                { x: 1 }"#,
            );
        let ws = build(
            "/abs/a".to_string(),
            r#"#import * from "./b"
            #import * from "./c"
            { combined: 1 }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(
            ws.workspace_diagnostics
                .iter()
                .any(|d| matches!(d, WorkspaceDiagnostic::CrossModuleSchemaCollision { name, .. } if name == "User")),
            "{:?}",
            ws.workspace_diagnostics
        );
    }

    #[test]
    fn cycle_diagnostic_chain_starts_and_ends_with_back_edge_target() {
        // Test #2 detail: chain shape is [target, ..., target].
        let mut loader = MapLoader::new();
        loader
            .add(
                "./a",
                "/abs/a",
                r#"#import b from "./b"
            { from_a: b }"#,
            )
            .add(
                "./b",
                "/abs/b",
                r#"#import a from "./a"
            { from_b: a }"#,
            );
        let ws = build(
            "/abs/a".to_string(),
            r#"#import b from "./b"
            { from_a: b }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        let cycle = ws
            .workspace_diagnostics
            .iter()
            .find_map(|d| match d {
                WorkspaceDiagnostic::CircularImport { chain, .. } => Some(chain.clone()),
                _ => None,
            })
            .expect("expected cycle");
        assert!(
            chain_is_well_formed(&cycle),
            "chain should start/end on the same id: {cycle:?}"
        );
    }

    fn chain_is_well_formed(chain: &[String]) -> bool {
        // A cycle chain is `[v0, v1, ..., vk, v0]` — same id at both
        // ends, length >= 2.
        chain.len() >= 2 && chain.first() == chain.last()
    }

    // === Stage 2.1: cross-module type resolution ===

    /// Helper: count `UnknownTypeName` diagnostics for a specific name
    /// across every module's per-tree diagnostics.
    fn unknown_type_count(ws: &WorkspaceTree, name: &str) -> usize {
        ws.modules
            .values()
            .flat_map(|t| t.diagnostics.iter())
            .filter(|d| matches!(d, Diagnostic::UnknownTypeName { name: n, .. } if n == name))
            .count()
    }

    #[test]
    fn spread_import_clears_unknown_type_for_main_param() {
        // Stage 2.1 forward: `#import * from "./lib"` exposes `LibType`
        // at the entry, so the entry's `#main(LibType x)` no longer
        // reports `UnknownTypeName`.
        let mut loader = MapLoader::new();
        loader.add(
            "./lib",
            "/abs/lib",
            r#"#schema LibType { Int id: * }
            { ok: 1 }"#,
        );
        let ws = build(
            "/abs/entry".to_string(),
            r#"#import * from "./lib"
            #main(LibType x)
            { ok: 1 }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert_eq!(
            unknown_type_count(&ws, "LibType"),
            0,
            "{:?}",
            ws.modules
                .get("/abs/entry")
                .map(|t| t.diagnostics.clone())
                .unwrap_or_default()
        );
    }

    #[test]
    fn destructure_import_clears_unknown_type_for_main_param() {
        // Stage 2.1: `#import { LibType } from "./lib"` should also expose
        // the name; alias-form `as LocalName` should expose under the alias.
        let mut loader = MapLoader::new();
        loader.add(
            "./lib",
            "/abs/lib",
            r#"#schema LibType { Int id: * }
            #schema OtherType { Int j: * }
            { ok: 1 }"#,
        );
        let ws = build(
            "/abs/entry".to_string(),
            r#"#import { LibType, OtherType as Local } from "./lib"
            #main(LibType x, Local y)
            { ok: 1 }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert_eq!(unknown_type_count(&ws, "LibType"), 0);
        assert_eq!(unknown_type_count(&ws, "Local"), 0);
    }

    #[test]
    fn unknown_type_still_flags_when_no_import_exposes_it() {
        // Stage 2.1 reverse: a name that genuinely isn't anywhere stays
        // flagged after the post-pass.
        let mut loader = MapLoader::new();
        loader.add(
            "./lib",
            "/abs/lib",
            r#"#schema LibType { Int id: * }
            { ok: 1 }"#,
        );
        let ws = build(
            "/abs/entry".to_string(),
            r#"#import * from "./lib"
            #main(NotExist x)
            { ok: 1 }"#,
            PathBuf::from("/abs"),
            &mut loader,
        );
        assert!(
            unknown_type_count(&ws, "NotExist") >= 1,
            "expected NotExist to remain unresolved"
        );
    }
}
