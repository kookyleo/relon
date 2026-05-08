//! Semantic analysis layer for Relon.
//!
//! Sits between `relon-parser` (raw AST) and `relon-evaluator` (runtime
//! tree-walk). Responsibilities that this crate gradually absorbs from the
//! evaluator:
//!
//! * Schema desugar — `#schema Name Body` directives lowered to a
//!   `SchemaDef` IR.
//! * Diagnostic aggregation — collect every structural issue in one pass
//!   instead of fail-fast.
//! * (Future) Name resolution — bind `Reference { base, path }` to a
//!   stable target id.
//! * (Future) Module graph — pre-resolve `#import` paths.
//!
//! The output is an [`AnalyzedTree`] keyed by [`relon_parser::NodeId`], so
//! the AST itself stays immutable and consumers (evaluator, LSP, lint)
//! pick up just the side-tables they need.

pub mod cap;
pub(crate) mod capability_check;
pub(crate) mod decorator_names;
pub mod diagnostic;
pub(crate) mod directive_names;
pub(crate) mod infer;
pub(crate) mod main_return;
pub mod main_sig;
pub mod modules;
pub mod resolve;
pub mod root_schemas;
pub mod schema;
pub mod sig;
pub(crate) mod stdlib_signatures;
pub mod tree;
pub mod typecheck;
pub mod workspace;
mod workspace_build;

pub use cap::{Capabilities, NativeFnGate};
pub use diagnostic::{Diagnostic, Severity};
pub use main_sig::{MainParam, MainSignature};
pub use modules::ModuleImport;
pub use resolve::ResolvedRef;
pub use root_schemas::RootSchemaDecl;
pub use schema::{
    lower_schema_pure, BaseRef, EnumVariant, MetaDecoratorRef, SchemaDef, SchemaFieldDef,
};
pub use sig::{lookup_signature, type_node_generic, type_node_simple, FnParam, FnSignature};
pub use tree::AnalyzedTree;
pub use workspace::{
    analyze_entry, analyze_entry_with_options, LoadError, LoadedModule, ModuleLoader, Workspace,
    WorkspaceDiagnostic, WorkspaceDiagnosticItem, WorkspaceTree,
};

use relon_parser::Node;
use std::collections::{HashMap, HashSet};

/// Run every analyzer pass over `root` and return the aggregated tree.
///
/// Errors are collected into [`AnalyzedTree::diagnostics`] rather than
/// short-circuiting. Use [`AnalyzedTree::has_errors`] to decide whether
/// to continue to evaluation.
///
/// This is the legacy single-file entry point — it sees only the
/// evaluator's hardcoded stdlib name set when deciding whether a free
/// variable in a closure body is "probably a stdlib reference, not a
/// typo". Hosts that register additional native functions should drive
/// analysis through [`analyze_with_options`] so their fn names are also
/// treated as known.
pub fn analyze(root: &Node) -> AnalyzedTree {
    analyze_with_options(root, &AnalyzeOptions::default())
}

/// Run every analyzer pass over `root` with caller-supplied options
/// (currently just the host-registered native fn name set). Used by the
/// workspace pass and by hosts that want analyzer diagnostics to align
/// with their actual `Capabilities::allow_native_fn` grant.
pub fn analyze_with_options(root: &Node, options: &AnalyzeOptions) -> AnalyzedTree {
    let mut tree = AnalyzedTree::new();
    tree.host_fn_names = options.host_fn_names.clone();
    tree.host_fn_signatures = options.host_fn_signatures.clone();
    tree.host_fn_gates = options.host_fn_gates.clone();
    tree.caps = options.caps.clone();
    schema::collect_schemas(root, &mut tree);
    // Root-level `#schema A Body` directives must run after
    // `collect_schemas` so the dual-declaration collision check has the
    // field-form table to consult.
    root_schemas::collect_root_schemas(root, &mut tree);
    // Stage 2.8: schema field types must be either a builtin / prelude
    // name or a declared schema. Runs after both schema-collecting
    // passes so the known-name set is fully populated.
    schema::check_schema_field_types(&mut tree);
    main_sig::collect_main(root, &mut tree);
    resolve::resolve_references(root, &mut tree);
    modules::collect_imports(root, &mut tree);
    typecheck::typecheck(root, &mut tree);
    // Stage 1.7: pre-flight check the entry's `#main(...) -> Type`
    // return annotation against the body's inferred type. Runs after
    // `typecheck` so the schema_index it relies on is fully populated
    // and any node-level `StaticTypeMismatch` already lives in
    // `tree.diagnostics`.
    main_return::check_main_return(root, &mut tree);
    tree
}

/// Caller-supplied hooks driving analyzer behavior. Stage 2.4
/// introduces this struct as the typed seam for "host knows more than
/// the analyzer can derive from source alone" — currently just the
/// allowlist of native fn names that should not be flagged as
/// `UnresolvedReference` when used as a free variable in a closure
/// body.
#[derive(Debug, Default, Clone)]
pub struct AnalyzeOptions {
    /// Names registered with the host's `Context::functions`. Empty by
    /// default — hosts that want their custom fn names recognized must
    /// populate this from their `Capabilities::allow_native_fn` set or
    /// from the keys of `Context::functions` directly.
    pub host_fn_names: HashSet<String>,
    /// Stage 3.4: signatures the host has declared for its native fns.
    /// When supplied, the FnCall checker validates arity / arg types
    /// against these signatures (same machinery the stdlib table
    /// drives). Names without a signature continue to participate only
    /// in the `host_fn_names` allowlist (silent on FnCall checking).
    pub host_fn_signatures: HashMap<String, FnSignature>,
    /// Stage 4: capability requirements declared by each registered
    /// native fn (mirror of the evaluator's `NativeFnGate` table). A
    /// missing entry means the fn was registered ungated (i.e. via
    /// `register_fn` not `register_fn_with_caps`) — the static check
    /// stays silent on it. Hosts populate this from the `gate` argument
    /// they passed to `register_fn_with_caps`.
    pub host_fn_gates: HashMap<String, cap::NativeFnGate>,
    /// Stage 4: the host's actual capability grant (mirror of the
    /// evaluator's `Capabilities`). Used by the static reachability
    /// check to decide whether a gated fn would be denied at runtime.
    /// Defaults to zero-trust — same as the evaluator default.
    pub caps: cap::Capabilities,
}
