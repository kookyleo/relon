//! Workspace-mode diagnostics for the LSP.
//!
//! Drives `relon-analyzer`'s `analyze_entry` over the edited file plus
//! every transitive `#import` it reaches, then maps the resulting
//! per-module + workspace-level diagnostics back to LSP diagnostics
//! grouped by file URI. The split from `server` keeps the workspace
//! plumbing testable without a transport.
//!
//! The loader chain mirrors `relon::FacadeLoader` / the CLI's
//! `CliLoader` but is constrained to the LSP workspace root rather than
//! using the trusted (unrestricted) filesystem resolver: the LSP runs
//! in editor-trust, which is narrower than CLI trust.

use crate::diagnostics::to_lsp;
use crate::position::offset_to_position;
use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString, Url};
use relon::{RuntimeError, Scope};
use relon_analyzer::workspace::{LoadError, LoadedModule, ModuleLoader};
use relon_analyzer::{analyze_entry, WorkspaceDiagnostic, WorkspaceTree};
// LSP runs a root-constrained filesystem resolver
// (`with_root_dir`) and consumes the `ModuleResolver` /
// `ModuleSource` traits to plumb the custom resolver chain into the
// analyzer workspace pass. These types live in the
// `relon-evaluator` impl crate and the facade deliberately does
// not re-export them — drive a `Box<dyn Evaluator>` through
// `relon::EvaluatorBuilder` if the consumer just wants a runtime,
// or take the direct reach (here) when a custom resolver chain is
// required.
use relon_evaluator::module::{
    FilesystemModuleResolver, ModuleResolver, ModuleSource, StdModuleResolver,
};
use relon_parser::TokenRange;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `ModuleLoader` impl backing the LSP's workspace pass. Mirrors
/// `relon::FacadeLoader` but with a root-constrained filesystem
/// resolver, since the LSP runs in editor trust (narrower than CLI).
pub struct LspLoader {
    resolvers: Vec<Arc<dyn ModuleResolver>>,
}

impl LspLoader {
    /// Build a loader rooted at `workspace_root`. `std/...` virtual
    /// modules are always served, and any disk reads are constrained
    /// to subpaths of `workspace_root` (canonicalized).
    pub fn new_for_workspace(workspace_root: PathBuf) -> Self {
        Self {
            resolvers: vec![
                Arc::new(StdModuleResolver),
                Arc::new(FilesystemModuleResolver::with_root_dir(workspace_root)),
            ],
        }
    }
}

impl ModuleLoader for LspLoader {
    fn load(&mut self, path: &str, current_dir: &Path) -> Result<LoadedModule, LoadError> {
        // Synthetic scope carrying just `current_dir` — same trick the
        // facade / CLI loaders use to bridge the analyzer trait (which
        // doesn't know about `Scope`) to the evaluator-side resolver
        // chain (which reads `current_dir` off the scope).
        let scope = Arc::new(Scope {
            current_dir: current_dir.to_string_lossy().into_owned().into(),
            ..Scope::default()
        });
        for resolver in &self.resolvers {
            match resolver.resolve(path, &scope, TokenRange::default()) {
                Ok(Some(ModuleSource {
                    canonical_id,
                    source,
                    current_dir: src_dir,
                })) => {
                    let dir = if src_dir.is_empty() {
                        current_dir.to_path_buf()
                    } else {
                        PathBuf::from(&src_dir)
                    };
                    return Ok(LoadedModule {
                        canonical_id,
                        source,
                        current_dir: dir,
                    });
                }
                Ok(None) => continue,
                Err(RuntimeError::CapabilityDenied { reason, .. }) => {
                    return Err(LoadError::AccessDenied(reason));
                }
                Err(RuntimeError::ModuleNotFound(_, _)) => {
                    return Err(LoadError::NotFound);
                }
                Err(other) => {
                    return Err(LoadError::Other(other.to_string()));
                }
            }
        }
        Err(LoadError::NotFound)
    }
}

/// Run a workspace-mode analyze starting at `entry_id` and return one
/// LSP diagnostic batch per file URI. The returned map covers:
///
/// * The entry — its per-module `tree.diagnostics` plus any
///   workspace-level diagnostics that hit an import inside it.
/// * Every transitive `#import` target the workspace pass loaded — its
///   own per-module diagnostics, plus workspace-level diagnostics
///   ascribed to it (because it is part of a cycle, or one of its
///   `#import`s couldn't be resolved).
///
/// Sources are needed for offset → line/column translation: the entry
/// owns its own buffer (we have it from the document store), and the
/// workspace pass holds the source text for every other module on
/// disk so we read it back from `WorkspaceTree::nodes` indirectly via
/// the loader's `LoadedModule` … which is consumed during the BFS.
/// Instead we re-read the file from disk here for non-entry modules;
/// it's `O(modules)` of read once per analysis, which the LSP runs
/// only on document edits.
pub fn compute_workspace_diagnostics(
    entry_uri: &Url,
    entry_canonical: &str,
    entry_source: &str,
    entry_dir: PathBuf,
    workspace_root: PathBuf,
) -> HashMap<Url, Vec<LspDiagnostic>> {
    let (_workspace, diags) = compute_workspace(
        entry_uri,
        entry_canonical,
        entry_source,
        entry_dir,
        workspace_root,
    );
    diags
}

/// Same workspace build as [`compute_workspace_diagnostics`] but also
/// returns the underlying [`WorkspaceTree`] for callers (the LSP
/// server) that need to consult its module graph from later request
/// handlers — go-to-definition reads
/// [`relon_analyzer::AnalyzedTree::cross_module_references`] to walk
/// `#import` jumps across files.
pub fn compute_workspace(
    entry_uri: &Url,
    entry_canonical: &str,
    entry_source: &str,
    entry_dir: PathBuf,
    workspace_root: PathBuf,
) -> (
    relon_analyzer::WorkspaceTree,
    HashMap<Url, Vec<LspDiagnostic>>,
) {
    let mut loader = LspLoader::new_for_workspace(workspace_root);
    let workspace = analyze_entry(
        entry_canonical.to_string(),
        entry_source,
        entry_dir,
        &mut loader,
    );
    let diags = map_workspace_to_lsp(&workspace, entry_uri, entry_canonical, entry_source);
    (workspace, diags)
}

/// Translate a `WorkspaceTree` plus its sources into LSP diagnostics
/// grouped by file URI. Pulled out as a standalone function so unit
/// tests can drive synthetic workspaces without going through
/// `analyze_entry`.
fn map_workspace_to_lsp(
    workspace: &WorkspaceTree,
    entry_uri: &Url,
    entry_canonical: &str,
    entry_source: &str,
) -> HashMap<Url, Vec<LspDiagnostic>> {
    let mut sources: HashMap<String, String> = HashMap::new();
    sources.insert(entry_canonical.to_string(), entry_source.to_string());
    // Read every other module from disk so we can render its labelled
    // ranges. Modules created from the entry buffer (only one per
    // analyze) reuse the in-memory text. We deliberately re-read on
    // each call: the LSP runs `compute_workspace_diagnostics` on every
    // edit, but only of the entry — the imported files are stable for
    // the duration of one analyze.
    for canonical_id in workspace.modules.keys() {
        if canonical_id == entry_canonical {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(canonical_id) {
            sources.insert(canonical_id.clone(), text);
        }
    }

    // Resolve URIs once. The entry's URI is provided by the caller (we
    // can't always reconstruct it from the canonical id — Windows
    // drive letters etc.); other modules use canonical filesystem
    // paths, which `Url::from_file_path` handles.
    let mut uris: HashMap<String, Url> = HashMap::new();
    uris.insert(entry_canonical.to_string(), entry_uri.clone());
    for canonical_id in workspace.modules.keys() {
        if canonical_id == entry_canonical {
            continue;
        }
        if let Ok(uri) = Url::from_file_path(canonical_id) {
            uris.insert(canonical_id.clone(), uri);
        }
    }

    let mut out: HashMap<Url, Vec<LspDiagnostic>> = HashMap::new();
    for url in uris.values() {
        out.entry(url.clone()).or_default();
    }

    // Per-module analyzer diagnostics — each is already scoped to the
    // file it was discovered in.
    for (canonical_id, tree) in &workspace.modules {
        let Some(uri) = uris.get(canonical_id) else {
            continue;
        };
        let Some(source) = sources.get(canonical_id) else {
            continue;
        };
        let diags = out.entry(uri.clone()).or_default();
        for d in &tree.diagnostics {
            diags.push(to_lsp(d, source));
        }
    }

    // Workspace-level diagnostics — ascribe each one to whichever
    // module(s) it materially relates to. Cycles fan out across every
    // edge in the chain so both ends of a 2-file cycle light up.
    for ws_diag in &workspace.workspace_diagnostics {
        attach_workspace_diag(ws_diag, workspace, &uris, &sources, &mut out);
    }

    out
}

/// Place a single `WorkspaceDiagnostic` onto the right file(s).
///
/// Strategy per variant:
///
/// * `CircularImport` — annotate every importer in the chain at the
///   `#import` directive that closes its outgoing edge, so a 2-file
///   `A <-> B` cycle shows up in both files.
/// * `ModuleNotFound` / `ModuleParseError` — annotate the importer
///   whose `#import` directive owns the diagnostic's range. Falls back
///   to the entry if no match is found (defensive; should not happen
///   for well-formed graphs).
/// * `CrossModuleSchemaCollision` — annotate only the importer whose
///   `#import *` surfaces the conflict; the conflict's range lives
///   there by construction.
fn attach_workspace_diag(
    diag: &WorkspaceDiagnostic,
    workspace: &WorkspaceTree,
    uris: &HashMap<String, Url>,
    sources: &HashMap<String, String>,
    out: &mut HashMap<Url, Vec<LspDiagnostic>>,
) {
    match diag {
        WorkspaceDiagnostic::CircularImport { chain, .. } => {
            // Walk consecutive (importer, target) pairs in the chain.
            // For each pair, find the importing directive in `importer`
            // whose target equals `target`, and emit a diagnostic at
            // that range on `importer`. The chain's last element
            // duplicates the first by construction (a -> b -> a), so
            // every back-edge is exercised exactly once.
            for window in chain.windows(2) {
                let (importer, target) = (&window[0], &window[1]);
                let Some(uri) = uris.get(importer) else {
                    continue;
                };
                let Some(source) = sources.get(importer) else {
                    continue;
                };
                let Some(range) = locate_import_range(workspace, importer, target) else {
                    continue;
                };
                let lsp_range = lsp_types::Range {
                    start: offset_to_position(source, range.0),
                    end: offset_to_position(source, range.1),
                };
                out.entry(uri.clone())
                    .or_default()
                    .push(build_ws_diagnostic(
                        lsp_range,
                        "relon::workspace::circular_import",
                        diag.to_string(),
                    ));
            }
        }
        WorkspaceDiagnostic::ModuleNotFound { .. }
        | WorkspaceDiagnostic::ModuleParseError { .. }
        | WorkspaceDiagnostic::CrossModuleSchemaCollision { .. }
        | WorkspaceDiagnostic::ImportHashMismatch { .. }
        | WorkspaceDiagnostic::ImportHashRequired { .. }
        | WorkspaceDiagnostic::ImportHashUnknownAlgorithm { .. }
        | WorkspaceDiagnostic::ImportHashInvalidHex { .. } => {
            // Find the module whose imports list owns this diagnostic's
            // range. Match by exact `(start, end)` byte equality —
            // every `WorkspaceDiagnostic` we currently emit was built
            // from a `ModuleImport::range`, so the round-trip is
            // lossless.
            let span = primary_span(diag);
            let Some(target_id) = find_owning_module(workspace, span) else {
                return;
            };
            let Some(uri) = uris.get(&target_id) else {
                return;
            };
            let Some(source) = sources.get(&target_id) else {
                return;
            };
            let lsp_range = lsp_types::Range {
                start: offset_to_position(source, span.0),
                end: offset_to_position(source, span.1),
            };
            let code = match diag {
                WorkspaceDiagnostic::ModuleNotFound { .. } => "relon::workspace::module_not_found",
                WorkspaceDiagnostic::ModuleParseError { .. } => {
                    "relon::workspace::module_parse_error"
                }
                WorkspaceDiagnostic::CrossModuleSchemaCollision { .. } => {
                    "relon::workspace::cross_module_schema_collision"
                }
                WorkspaceDiagnostic::ImportHashMismatch { .. } => {
                    "relon::workspace::import_hash_mismatch"
                }
                WorkspaceDiagnostic::ImportHashRequired { .. } => {
                    "relon::workspace::import_hash_required"
                }
                WorkspaceDiagnostic::ImportHashUnknownAlgorithm { .. } => {
                    "relon::workspace::import_hash_unknown_algorithm"
                }
                WorkspaceDiagnostic::ImportHashInvalidHex { .. } => {
                    "relon::workspace::import_hash_invalid_hex"
                }
                _ => unreachable!(),
            };
            out.entry(uri.clone())
                .or_default()
                .push(build_ws_diagnostic(lsp_range, code, diag.to_string()));
        }
    }
}

/// Build an ERROR-severity workspace `LspDiagnostic`, filling in the
/// constant fields (`source = "relon"`, no code description / related
/// info / tags / data) so the call sites only supply the parts that
/// vary per diagnostic variant.
fn build_ws_diagnostic(range: lsp_types::Range, code: &str, message: String) -> LspDiagnostic {
    LspDiagnostic {
        range,
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String(code.to_string())),
        code_description: None,
        source: Some("relon".to_string()),
        message,
        related_information: None,
        tags: None,
        data: None,
    }
}

/// Pull the `(start, end)` byte range out of a workspace diagnostic.
/// Each variant carries a `SourceSpan` field; this central accessor
/// keeps the `match` in one place.
fn primary_span(diag: &WorkspaceDiagnostic) -> (usize, usize) {
    let span = match diag {
        WorkspaceDiagnostic::CircularImport { range, .. } => *range,
        WorkspaceDiagnostic::ModuleNotFound { range, .. } => *range,
        WorkspaceDiagnostic::ModuleParseError { range, .. } => *range,
        WorkspaceDiagnostic::CrossModuleSchemaCollision { range, .. } => *range,
        WorkspaceDiagnostic::ImportHashMismatch { range, .. } => *range,
        WorkspaceDiagnostic::ImportHashRequired { range, .. } => *range,
        WorkspaceDiagnostic::ImportHashUnknownAlgorithm { range, .. } => *range,
        WorkspaceDiagnostic::ImportHashInvalidHex { range, .. } => *range,
    };
    (span.offset(), span.offset() + span.len())
}

/// Find the canonical id of the module whose `imports` list contains a
/// directive at the given byte range. Used to attach
/// non-`CircularImport` workspace diagnostics — those carry the
/// importer's `#import` range but not the importer's id.
fn find_owning_module(workspace: &WorkspaceTree, span: (usize, usize)) -> Option<String> {
    for (id, tree) in &workspace.modules {
        for imp in &tree.imports {
            let r = imp.range;
            let s = (r.start.offset, r.end.offset);
            if s == span {
                return Some(id.clone());
            }
        }
    }
    None
}

/// Find the `(start, end)` byte range of the `#import` directive in
/// `importer` that targets `target`. Mirrors the analyzer's
/// `locate_import_range` but works against the rebuilt graph keyed by
/// canonical id; we walk `import_graph` and `imports` in lockstep
/// because the analyzer rewrites graph edges to canonical ids only on
/// successful loads.
fn locate_import_range(
    workspace: &WorkspaceTree,
    importer: &str,
    target: &str,
) -> Option<(usize, usize)> {
    let tree = workspace.modules.get(importer)?;
    let edges = workspace.import_graph.get(importer)?;
    for (idx, edge) in edges.iter().enumerate() {
        if edge == target {
            let r = tree.imports.get(idx)?.range;
            return Some((r.start.offset, r.end.offset));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_parser::parse_document;
    use std::sync::Arc;

    /// Helper: write a transient temp directory containing the named
    /// files. Pattern mirrors `relon-evaluator::sandbox_tests` (no
    /// `tempfile` crate dependency, manual cleanup with a fresh PID-
    /// suffixed dir per test).
    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "relon-lsp-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let p = self.path.join(name);
            std::fs::write(&p, contents).unwrap();
            std::fs::canonicalize(&p).unwrap()
        }

        fn root(&self) -> PathBuf {
            std::fs::canonicalize(&self.path).unwrap()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn workspace_cycle_lights_up_both_files() {
        // Two-file cycle: a.relon imports b.relon, b.relon imports a.relon.
        let dir = TestDir::new("cycle");
        let a_path = dir.write(
            "a.relon",
            r#"#import b from "./b.relon"
{ x: 1 }"#,
        );
        let b_path = dir.write(
            "b.relon",
            r#"#import a from "./a.relon"
{ y: 2 }"#,
        );
        let a_source = std::fs::read_to_string(&a_path).unwrap();

        let entry_uri = Url::from_file_path(&a_path).unwrap();
        let entry_canonical = a_path.to_string_lossy().to_string();
        let by_uri = compute_workspace_diagnostics(
            &entry_uri,
            &entry_canonical,
            &a_source,
            a_path.parent().unwrap().to_path_buf(),
            dir.root(),
        );

        let b_uri = Url::from_file_path(&b_path).unwrap();
        let a_diags = by_uri.get(&entry_uri).expect("entry diagnostics");
        let b_diags = by_uri.get(&b_uri).expect("imported file diagnostics");

        assert!(
            a_diags
                .iter()
                .any(|d| d.message.contains("circular import")),
            "a diags: {a_diags:?}"
        );
        assert!(
            b_diags
                .iter()
                .any(|d| d.message.contains("circular import")),
            "b diags: {b_diags:?}"
        );
    }

    #[test]
    fn imported_module_schema_error_lights_up_imported_file() {
        // a.relon imports b.relon; b.relon has its own analyzer error.
        // The entry (a) should be diagnostic-clean modulo the import,
        // and b's URI should carry the schema diagnostic.
        let dir = TestDir::new("imported-error");
        let a_path = dir.write(
            "a.relon",
            r#"#import b from "./b.relon"
{ ok: 1 }"#,
        );
        let b_path = dir.write("b.relon", r#"{ #schema Bad 42 }"#);
        let a_source = std::fs::read_to_string(&a_path).unwrap();

        let entry_uri = Url::from_file_path(&a_path).unwrap();
        let entry_canonical = a_path.to_string_lossy().to_string();
        let by_uri = compute_workspace_diagnostics(
            &entry_uri,
            &entry_canonical,
            &a_source,
            a_path.parent().unwrap().to_path_buf(),
            dir.root(),
        );

        let b_uri = Url::from_file_path(&b_path).unwrap();
        let b_diags = by_uri.get(&b_uri).expect("imported file diagnostics");
        assert!(
            b_diags
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "expected error in b.relon, got {b_diags:?}"
        );
    }

    #[test]
    fn module_not_found_attaches_to_importer() {
        // Entry imports a non-existent module; we expect the
        // diagnostic on the entry URI at the `#import` range.
        let dir = TestDir::new("missing");
        let a_path = dir.write(
            "a.relon",
            r#"#import gone from "./does_not_exist.relon"
{ x: 1 }"#,
        );
        let a_source = std::fs::read_to_string(&a_path).unwrap();

        let entry_uri = Url::from_file_path(&a_path).unwrap();
        let entry_canonical = a_path.to_string_lossy().to_string();
        let by_uri = compute_workspace_diagnostics(
            &entry_uri,
            &entry_canonical,
            &a_source,
            a_path.parent().unwrap().to_path_buf(),
            dir.root(),
        );

        let a_diags = by_uri.get(&entry_uri).expect("entry diagnostics");
        assert!(
            a_diags
                .iter()
                .any(|d| d.message.contains("module not found")),
            "expected module not found, got {a_diags:?}"
        );
    }

    /// Driving `map_workspace_to_lsp` with no imports: we should still
    /// surface the entry's own analyzer diagnostics. Kept as a
    /// regression for the fallback path (entry-only workspace).
    #[test]
    fn entry_only_workspace_returns_per_module_diags() {
        let entry_canonical = "synthetic://entry".to_string();
        let entry_source = "{ #schema Bad 42 }";
        let entry_uri = Url::parse("file:///synthetic/entry.relon").unwrap();
        let node = parse_document(entry_source).unwrap();
        let tree = relon_analyzer::analyze(&node);
        let mut workspace = WorkspaceTree::new();
        workspace.entry_id = entry_canonical.clone();
        workspace
            .modules
            .insert(entry_canonical.clone(), Arc::new(tree));
        workspace
            .import_graph
            .insert(entry_canonical.clone(), Vec::new());

        let by_uri = map_workspace_to_lsp(&workspace, &entry_uri, &entry_canonical, entry_source);
        let diags = by_uri.get(&entry_uri).expect("entry diags");
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
            "expected schema error, got {diags:?}"
        );
    }
}
