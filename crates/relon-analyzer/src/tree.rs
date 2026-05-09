//! Side-tables that analyzer passes attach to a parsed AST.
//!
//! `AnalyzedTree` is keyed by [`relon_parser::NodeId`]. The AST itself is
//! not modified — consumers (evaluator, LSP) read the tables they need and
//! ignore the rest. Adding a new pass means adding a new table here.

use crate::cap::{Capabilities, NativeFnGate};
use crate::diagnostic::Diagnostic;
use crate::main_sig::MainSignature;
use crate::resolve::ResolvedRef;
use crate::root_schemas::RootSchemaDecl;
use crate::schema::SchemaDef;
use crate::sig::FnSignature;
use crate::workspace_build::WorkspaceImportIndex;
use relon_parser::{Node, NodeId};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Aggregated output of `analyze`. Cheap to construct, cheap to share via
/// `Arc` (none of the inner maps are large for typical config files).
#[derive(Debug, Default)]
pub struct AnalyzedTree {
    /// Schema definitions discovered by the schema pass, keyed by the
    /// `NodeId` of the schema body node carried by `#schema A Body`.
    pub schemas: HashMap<NodeId, SchemaDef>,
    /// Statically resolvable references, keyed by the reference
    /// expression's `NodeId`. Populated by `resolve_references`. Hosts
    /// (LSP, type-checker, lint) join this against `node_index` to map
    /// from a reference site back to the field it points at.
    pub references: HashMap<NodeId, ResolvedRef>,
    /// Snapshot of every `NodeId`-bearing AST node visited by an
    /// analyzer pass. Lets consumers turn a `NodeId` (returned by
    /// `references` or `schemas`) back into the original `&Node`
    /// without holding the parser tree alive themselves.
    pub node_index: HashMap<NodeId, Arc<Node>>,
    /// Module imports discovered by the module-graph pass.
    pub imports: Vec<crate::modules::ModuleImport>,
    /// `#main(...)` signature on the root node, when the file is an
    /// entry program. Files without `#main` are libraries / static
    /// configs; the host evaluates them through `eval_root` rather than
    /// `run_main`.
    pub main_signature: Option<MainSignature>,
    /// Root-level `#schema Name Body` declarations in source order.
    /// Each entry seeds `Name` into the root scope before evaluation
    /// begins. Multiple decorations naming the same schema produce
    /// `Diagnostic::DuplicateRootSchemaName`; same name as a dict-field
    /// `#schema X ...` produces
    /// `Diagnostic::RootSchemaCollidesWithField`.
    pub root_schemas: Vec<RootSchemaDecl>,
    /// Errors and warnings from every pass, in source order.
    pub diagnostics: Vec<Diagnostic>,
    /// Stage 2.4: the host-registered native fn names known when this
    /// tree was analyzed. Used by `typecheck` to avoid flagging
    /// `host_fn(...)` calls as `UnresolvedReference`. Empty for the
    /// legacy single-file `analyze` entry; populated when the caller
    /// drives analysis through `analyze_with_options`.
    pub host_fn_names: HashSet<String>,
    /// Stage 3: signatures the host has supplied for its native fns
    /// (via `AnalyzeOptions::host_fn_signatures`). Looked up by
    /// `lookup_signature` to drive `FnCall` arity / type checks for
    /// custom fns, in the same way the stdlib table covers builtins.
    pub host_fn_signatures: HashMap<String, FnSignature>,
    /// Stage 3.3: signatures of every user closure encountered in
    /// source. Keyed by the closure's `Expr::Closure` AST `NodeId`.
    /// Populated by the type-check walker when it enters each closure.
    pub closure_signatures: HashMap<NodeId, FnSignature>,
    /// Stage 3.3: index from a dict-field name to the `NodeId` of the
    /// closure value bound to it. Lets `lookup_signature` find the
    /// signature for a sibling closure call without re-walking the
    /// scope chain.
    pub field_closure_index: HashMap<String, NodeId>,
    /// Stage 4: per-fn capability requirements supplied by the host
    /// (mirror of `relon_evaluator::eval::NativeFnGate`). Drives the
    /// static reachability check — a gated fn called from a reachable
    /// site whose required cap isn't in `caps` produces
    /// `Diagnostic::CapabilityRequired`.
    pub host_fn_gates: HashMap<String, NativeFnGate>,
    /// Stage 4: the context-wide capability grant the host plans to
    /// hand the evaluator. Compared against `host_fn_gates` during the
    /// reachability check.
    pub caps: Capabilities,
    /// v1.1: cross-module import index, populated by the workspace
    /// build pass after every reachable module is analyzed. `None` for
    /// trees produced by the single-file `analyze` entry point — that
    /// path has no module graph to consult. Consumed by
    /// [`crate::sig::lookup_signature`] to resolve calls to closure
    /// signatures from `#import`ed modules.
    pub workspace_import_index: Option<WorkspaceImportIndex>,
    /// v1.3: when `true`, this module was analyzed under strict mode.
    /// Either the module declared `#strict` directly, or the workspace
    /// pass propagated the flag from a strict entry's `#import` graph
    /// (transitive). Drives the analyzer's no-silent-fallback policy
    /// during inference.
    pub strict_mode: bool,
}

impl AnalyzedTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if any diagnostic at `Error` severity was emitted. Hosts
    /// typically gate evaluation on this.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity() == crate::diagnostic::Severity::Error)
    }

    /// Drain diagnostics, leaving the tree's other tables intact.
    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    /// Look up a desugar'd schema by the `#schema`-body node's id.
    pub fn schema(&self, node_id: NodeId) -> Option<&SchemaDef> {
        self.schemas.get(&node_id)
    }

    /// Resolve a reference site (the `Reference { ... }` or
    /// `Variable(...)` expression node id) to the field it statically
    /// binds to.
    pub fn reference(&self, node_id: NodeId) -> Option<&ResolvedRef> {
        self.references.get(&node_id)
    }

    /// Recover the original `Node` (snapshot) for a `NodeId` returned
    /// by any other side-table.
    pub fn node(&self, node_id: NodeId) -> Option<&Arc<Node>> {
        self.node_index.get(&node_id)
    }
}
