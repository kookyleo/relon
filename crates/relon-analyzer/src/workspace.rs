//! Workspace-level analyzer output.
//!
//! `WorkspaceTree` is to `AnalyzedTree` what a multi-file build is to a
//! single-file lint pass. It bundles every module reached from the entry
//! file (entry + transitive imports) into one structure, plus the
//! cross-module diagnostics that only make sense once the import graph
//! exists (cycles, missing modules, cross-module schema collisions).
//!
//! Hosts that drive evaluation feed the resulting `WorkspaceTree` to
//! `Context::with_workspace`, which lets the evaluator skip per-module
//! parse + analyze on the hot path.
//!
//! Existing single-file consumers (LSP, the legacy `Workspace::find_*`
//! helpers below) keep working: `WorkspaceTree::modules` carries the
//! same `HashMap<String, Arc<AnalyzedTree>>` shape that `Workspace::files`
//! used to expose.

use crate::diagnostic::{Diagnostic, Severity};
use crate::tree::AnalyzedTree;
use miette::{Diagnostic as MietteDiagnostic, SourceSpan};
use relon_parser::{Node, NodeId, TokenRange};
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;

/// Aggregated output of `analyze_entry`. Holds:
///
/// * `entry_id` — canonical id of the file the host asked for.
/// * `modules` — analyzed side-tables for entry + every reachable
///   module, keyed by canonical id (same key the evaluator's module
///   cache uses, so the runtime can look up an already-analyzed tree).
/// * `nodes` — parsed root `Node` per module, kept alongside
///   `modules` so the evaluator never has to re-parse a module the
///   workspace pass already touched.
/// * `import_graph` — adjacency list of `canonical_id -> [imported_id]`,
///   useful for tooling ("show import graph") and for cycle reporting
///   that needs to enumerate paths.
/// * `workspace_diagnostics` — errors that only make sense across
///   module boundaries (cycle, missing module, parse error in an
///   imported file, schema collision under spread imports).
#[derive(Default)]
pub struct WorkspaceTree {
    pub entry_id: String,
    pub modules: HashMap<String, Arc<AnalyzedTree>>,
    pub nodes: HashMap<String, Arc<Node>>,
    pub import_graph: HashMap<String, Vec<String>>,
    pub workspace_diagnostics: Vec<WorkspaceDiagnostic>,
    /// v1.3: `true` when the entry module (or any caller-forwarded
    /// `AnalyzeOptions::strict_mode`) declared strict mode. Mirrored
    /// onto every reachable module's `AnalyzedTree::strict_mode` by
    /// the workspace build pass so contagion is observable from a
    /// single field.
    pub strict_mode: bool,
}

impl WorkspaceTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if any diagnostic at `Error` severity was emitted, either at
    /// the workspace level (cycle, missing module, …) or inside any
    /// module's own `AnalyzedTree`. Hosts gate evaluation on this.
    pub fn has_errors(&self) -> bool {
        if self
            .workspace_diagnostics
            .iter()
            .any(|d| d.severity() == Severity::Error)
        {
            return true;
        }
        self.modules.values().any(|tree| tree.has_errors())
    }

    /// The entry file's analyzed side-table, when present. Missing only
    /// if `analyze_entry` failed before the entry's own analyze pass
    /// could populate `modules` — currently never reachable, but keeping
    /// the `Option` return makes future error-recovery paths cheaper.
    pub fn entry_tree(&self) -> Option<&AnalyzedTree> {
        self.modules.get(&self.entry_id).map(|arc| &**arc)
    }

    /// The entry file's parsed root node, paired with `entry_tree`.
    pub fn entry_node(&self) -> Option<&Arc<Node>> {
        self.nodes.get(&self.entry_id)
    }

    /// Iterate over every error diagnostic in the workspace, both
    /// workspace-level and per-module. Caller-side display loops can
    /// drive a single rendering pipeline without forking on the source
    /// tag.
    pub fn all_error_diagnostics(&self) -> Vec<WorkspaceDiagnosticItem<'_>> {
        let mut out = Vec::new();
        for d in &self.workspace_diagnostics {
            if d.severity() == Severity::Error {
                out.push(WorkspaceDiagnosticItem::Workspace(d));
            }
        }
        for (id, tree) in &self.modules {
            for d in &tree.diagnostics {
                if d.severity() == Severity::Error {
                    out.push(WorkspaceDiagnosticItem::Module {
                        canonical_id: id.as_str(),
                        diag: d,
                    });
                }
            }
        }
        out
    }
}

/// A diagnostic rendered with its source location: either the workspace
/// itself (no specific module owns it, e.g. a cycle spans many files) or
/// a single analyzed module.
#[derive(Debug, Clone, Copy)]
pub enum WorkspaceDiagnosticItem<'a> {
    Workspace(&'a WorkspaceDiagnostic),
    Module {
        canonical_id: &'a str,
        diag: &'a Diagnostic,
    },
}

/// Errors that only make sense once you have a module graph: cycles,
/// missing files, parse failures in imported modules, cross-module
/// name collisions surfaced by `#import *`.
#[derive(Debug, Clone, Error, MietteDiagnostic)]
pub enum WorkspaceDiagnostic {
    #[error("circular import: {}", chain.join(" -> "))]
    #[diagnostic(
        code(relon::workspace::circular_import),
        help(
            "Break the cycle by removing one of the `#import` directives or by extracting the shared definitions into a third module that neither side imports back from."
        )
    )]
    CircularImport {
        chain: Vec<String>,
        #[label("import that closes the cycle")]
        range: SourceSpan,
    },

    #[error("module not found: {path}")]
    #[diagnostic(
        code(relon::workspace::module_not_found),
        help(
            "Check the import path. Built-in modules are spelled `std/<name>`; relative paths resolve against the importing file's directory."
        )
    )]
    ModuleNotFound {
        path: String,
        #[label("imported here")]
        range: SourceSpan,
    },

    #[error("module parse error in `{path}`: {message}")]
    #[diagnostic(
        code(relon::workspace::module_parse_error),
        help(
            "The imported file has a syntactic problem. Run the analyzer / formatter on the file directly to see the position-bearing parse error."
        )
    )]
    ModuleParseError {
        path: String,
        message: String,
        #[label("imported here")]
        range: SourceSpan,
    },

    #[error("schema `{name}` defined in both `{first}` and `{second}` (both spread-imported)")]
    #[diagnostic(
        code(relon::workspace::cross_module_schema_collision),
        help(
            "Two `#import *` modules export a top-level schema with the same name. Either rename one of them, or replace one of the spreads with an alias / destructure import that hides the conflicting binding."
        )
    )]
    CrossModuleSchemaCollision {
        name: String,
        first: String,
        second: String,
        #[label("conflict surfaces here")]
        range: SourceSpan,
    },

    /// v3++ b-2: the loaded source for an `#import` whose directive
    /// carries `<algo>:"<hex>"` did not match the pinned digest.
    /// Surfaced before the module enters the workspace so a poisoned
    /// upstream cannot reach analysis or evaluation.
    #[error("import `{path}` hash mismatch: expected {expected}, got {got}")]
    #[diagnostic(
        code(relon::workspace::import_hash_mismatch),
        help(
            "The loaded module's digest differs from the pinned hash on the `#import` directive. Either update the pin to the new digest after auditing the change, or refuse to load the module."
        )
    )]
    ImportHashMismatch {
        path: String,
        algorithm: String,
        expected: String,
        got: String,
        #[label("pinned import does not match the loaded source")]
        range: SourceSpan,
    },

    /// v3++ b-2: `--require-hash` (or the equivalent host-side knob)
    /// was set and an `#import` was missing the integrity pin. Emitted
    /// for every unpinned import so the operator sees the full list
    /// in one analyzer pass rather than one error per re-run.
    #[error("import `{path}` is missing a required integrity hash")]
    #[diagnostic(
        code(relon::workspace::import_hash_required),
        help(
            "`--require-hash` enforces pinned `#import`s. Add an inline pin like `#import x from \"...\" sha256:\"<hex>\"`, or rerun without `--require-hash` for an unpinned environment."
        )
    )]
    ImportHashRequired {
        path: String,
        #[label("missing integrity pin")]
        range: SourceSpan,
    },

    /// v3++ b-2: the directive carried an integrity pin but the
    /// algorithm identifier was not one the analyzer knows about.
    /// Distinct from `ImportHashMismatch` so the operator can tell
    /// "typo / unsupported algo" from "content drift".
    #[error("import `{path}` uses unknown integrity algorithm `{algorithm}`")]
    #[diagnostic(
        code(relon::workspace::import_hash_unknown_algorithm),
        help(
            "Supported algorithms: sha256. Future versions may add sha512 / blake3; until then, replace the pin with `sha256:\"<hex>\"`."
        )
    )]
    ImportHashUnknownAlgorithm {
        path: String,
        algorithm: String,
        #[label("unknown integrity algorithm")]
        range: SourceSpan,
    },

    /// v3++ b-2: the integrity hex string did not match the expected
    /// length for the declared algorithm (e.g. `sha256:"abc"`). Caught
    /// up front so the loader never compares against a partial digest.
    #[error(
        "import `{path}` integrity hex length is {got}, expected {expected} for {algorithm}"
    )]
    #[diagnostic(
        code(relon::workspace::import_hash_invalid_hex),
        help(
            "Integrity hex must be exactly the algorithm's digest length (sha256 = 64 hex chars). Re-generate the pin from the actual file digest."
        )
    )]
    ImportHashInvalidHex {
        path: String,
        algorithm: String,
        expected: usize,
        got: usize,
        #[label("malformed integrity hex")]
        range: SourceSpan,
    },
}

impl WorkspaceDiagnostic {
    pub fn severity(&self) -> Severity {
        // Every workspace-level finding currently blocks evaluation:
        // cycles will deadlock the evaluator's `loading_modules` guard,
        // missing / unparseable modules cannot be loaded, and
        // collisions render `#import *` ambiguous.
        Severity::Error
    }
}

/// Errors a `ModuleLoader` can return when asked to materialize a module.
#[derive(Debug, Clone)]
pub enum LoadError {
    /// The path doesn't refer to any module the loader knows about.
    /// Surfaced as `WorkspaceDiagnostic::ModuleNotFound`.
    NotFound,
    /// The loader could find the module but is forbidden from reading
    /// it (sandbox / capability denied). Surfaced as
    /// `ModuleNotFound` with the reason in the help text — the
    /// distinction matters for tooling but not for the user-facing
    /// "this import won't work" message.
    AccessDenied(String),
    /// Any other I/O or loader-internal failure.
    Other(String),
}

/// Source text + identity for a module the workspace pass loaded.
#[derive(Debug, Clone)]
pub struct LoadedModule {
    /// Stable identity. Must match the evaluator's
    /// `ModuleSource::canonical_id` so the runtime can look up the
    /// pre-analyzed tree by the same key.
    pub canonical_id: String,
    pub source: String,
    /// Working directory used when this module's own `#import "./..."`
    /// directives are resolved.
    pub current_dir: std::path::PathBuf,
}

/// Pluggable module fetcher used by `analyze_entry`.
///
/// Kept as a trait (rather than a concrete `FilesystemModuleResolver`
/// reference) so the analyzer crate stays free of `std::fs` calls — the
/// facade / CLI / LSP each adapt their own resolver chain into a
/// `ModuleLoader` impl. The same trait is what makes the workspace
/// tests fully in-memory: the test loader is a `HashMap<String, String>`.
pub trait ModuleLoader {
    fn load(
        &mut self,
        path: &str,
        current_dir: &std::path::Path,
    ) -> Result<LoadedModule, LoadError>;
}

// Legacy single-file workspace API used by LSP-style cross-file
// reference search. Kept on `WorkspaceTree` so existing callers don't
// have to migrate; new callers should prefer `analyze_entry` + the
// workspace-level helpers above.
impl WorkspaceTree {
    pub fn add_file(&mut self, path: String, tree: AnalyzedTree) {
        self.modules.insert(path, Arc::new(tree));
    }

    /// Find all references to a specific node (definition) across the
    /// entire workspace.
    pub fn find_references(&self, target_id: NodeId) -> Vec<(String, TokenRange)> {
        let mut results = Vec::new();
        for (path, tree) in &self.modules {
            for resolved in tree.references.values() {
                if resolved.target == target_id {
                    results.push((path.clone(), resolved.source_range));
                }
            }
        }
        results
    }

    /// Find all references to a symbol exported from `from_path`.
    /// This follows `#import` chains.
    pub fn find_symbol_references(
        &self,
        _from_path: &str,
        _symbol_name: &str,
    ) -> Vec<(String, TokenRange)> {
        // Placeholder for future implementation.
        Vec::new()
    }
}

/// Backwards-compatible alias for the pre-Stage-0 `Workspace` type. The
/// LSP and a handful of internal callers still spell it `Workspace`;
/// re-exporting the new struct under the old name keeps them building
/// without forcing a churn change in this Stage.
pub type Workspace = WorkspaceTree;

/// Construct a workspace tree from `entry_id` + the entry source plus a
/// `ModuleLoader` for transitive imports. The function:
///
/// 1. Parses + analyzes the entry, recording any parse / analyzer
///    diagnostics on the entry's `AnalyzedTree`.
/// 2. BFS-walks `#import` declarations; for each unseen module, asks
///    `loader` for the source, parses + analyzes it, and enqueues its
///    own imports.
/// 3. Detects cycles via a DFS pre-pass over the resulting `import_graph`,
///    so that the cycle report uses graph data (not BFS state) and can
///    enumerate the closing chain in source-order.
/// 4. Aggregates errors instead of failing fast: a parse error in one
///    module does not stop other already-enqueued modules from being
///    analyzed.
///
/// The returned `WorkspaceTree::has_errors()` is the gate hosts use to
/// decide whether to invoke the evaluator. Implementation lives in
/// Stage 0.2; this stub keeps the public surface ready for callers.
pub fn analyze_entry<L: ModuleLoader>(
    entry_id: String,
    entry_source: &str,
    entry_current_dir: std::path::PathBuf,
    loader: &mut L,
) -> WorkspaceTree {
    crate::workspace_build::build(
        entry_id,
        entry_source,
        entry_current_dir,
        loader,
        &crate::AnalyzeOptions::default(),
    )
}

/// Same as [`analyze_entry`] but threads caller-supplied
/// [`crate::AnalyzeOptions`] (currently the host-registered fn name
/// allowlist) through to every per-module `analyze` call so closure
/// free-var diagnostics align with the host's actual capability grant.
pub fn analyze_entry_with_options<L: ModuleLoader>(
    entry_id: String,
    entry_source: &str,
    entry_current_dir: std::path::PathBuf,
    loader: &mut L,
    options: &crate::AnalyzeOptions,
) -> WorkspaceTree {
    crate::workspace_build::build(entry_id, entry_source, entry_current_dir, loader, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use crate::diagnostic::Diagnostic;
    use relon_parser::parse_document;

    fn dummy_span() -> SourceSpan {
        SourceSpan::from((0usize, 0usize))
    }

    #[test]
    fn empty_workspace_has_no_errors() {
        let ws = WorkspaceTree::new();
        assert!(!ws.has_errors());
        assert!(ws.entry_tree().is_none());
    }

    #[test]
    fn workspace_diagnostic_marks_errors() {
        let mut ws = WorkspaceTree::new();
        ws.workspace_diagnostics
            .push(WorkspaceDiagnostic::ModuleNotFound {
                path: "./missing.relon".to_string(),
                range: dummy_span(),
            });
        assert!(ws.has_errors());
        let errs = ws.all_error_diagnostics();
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], WorkspaceDiagnosticItem::Workspace(_)));
    }

    #[test]
    fn module_diagnostic_propagates_to_workspace() {
        let mut ws = WorkspaceTree::new();
        let mut tree = AnalyzedTree::new();
        // Inject a known Error-severity diagnostic so has_errors flips
        // even though no workspace-level entry exists.
        tree.diagnostics.push(Diagnostic::SchemaFieldUntyped {
            field: "x".to_string(),
            range: dummy_span(),
        });
        ws.modules.insert("a.relon".to_string(), Arc::new(tree));
        assert!(ws.has_errors());
        let errs = ws.all_error_diagnostics();
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0], WorkspaceDiagnosticItem::Module { .. }));
    }

    #[test]
    fn entry_tree_resolves_via_entry_id() {
        let mut ws = WorkspaceTree::new();
        ws.entry_id = "a.relon".to_string();
        let node = parse_document("{ x: 1 }").unwrap();
        let tree = analyze(&node);
        ws.modules.insert("a.relon".to_string(), Arc::new(tree));
        assert!(ws.entry_tree().is_some());
    }

    #[test]
    fn legacy_find_references_still_works() {
        // Keep the pre-Stage-0 cross-file reference search behavior
        // working under the renamed field. This is the only LSP-facing
        // promise the old `Workspace` shape made.
        let mut ws: Workspace = Workspace::new();

        let src_a = r#"{ shared_val: 42 }"#;
        let node_a = parse_document(src_a).unwrap();
        let tree_a = analyze(&node_a);
        let shared_val_id = if let relon_parser::Expr::Dict(pairs) = &*node_a.expr {
            pairs[0].1.id
        } else {
            panic!()
        };
        ws.add_file("a.relon".to_string(), tree_a);

        let src_b = r#"{ usage: 100 }"#;
        let node_b = parse_document(src_b).unwrap();
        let mut tree_b = analyze(&node_b);
        tree_b.references.insert(
            node_b.id,
            crate::resolve::ResolvedRef {
                target: shared_val_id,
                source_range: node_b.range,
                via: relon_parser::RefBase::Sibling,
            },
        );
        ws.add_file("b.relon".to_string(), tree_b);

        let refs = ws.find_references(shared_val_id);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "b.relon");
    }
}
