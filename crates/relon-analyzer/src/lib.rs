#![forbid(unsafe_code)]
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

// rustc ≥ 1.93 false-positive: `unused_assignments` fires on fields of
// every `#[derive(miette::Diagnostic)]` / `thiserror::Error` enum (the
// derive expands to internal let-bindings that the lint mis-reads).
// Upstream: <https://github.com/rust-lang/rust/issues/147648>
// (stable→stable regression, P-medium, still open). Drop this `allow`
// once the rustc fix lands.
#![allow(unused_assignments)]

pub(crate) mod ban_unsafe_types;
pub mod cap;
pub(crate) mod capability_check;
pub mod code_actions;
pub mod complete;
pub(crate) mod const_fold;
pub(crate) mod constraints;
pub(crate) mod core_schemas;
pub(crate) mod decorator_names;
pub mod diagnostic;
pub(crate) mod directive_names;
pub(crate) mod extend;
pub(crate) mod generics;
pub mod goto_def;
pub mod hover;
pub(crate) mod infer;
pub mod inlay_hints;
pub(crate) mod main_return;
pub mod main_sig;
pub mod modules;
pub mod references;
pub mod rename;
pub mod resolve;
pub mod root_schemas;
pub mod schema;
pub mod sig;
pub mod signature_help;
pub mod stdlib_signatures;
pub mod symbols;
pub mod tree;
pub mod typecheck;
pub mod workspace;
mod workspace_build;

pub use cap::{Capabilities, NativeFnGate};
pub use diagnostic::{Diagnostic, Severity};
pub use main_sig::{MainParam, MainSignature};
pub use modules::ModuleImport;
pub use resolve::{CrossModuleRef, CrossModuleVia, ResolvedRef};
pub use root_schemas::RootSchemaDecl;
pub use schema::{
    lower_schema_pure, BaseRef, EnumVariant, MetaDecoratorRef, SchemaDef, SchemaFieldDef,
};
pub use sig::{lookup_signature, type_node_generic, type_node_simple, FnParam, FnSignature};
pub use tree::AnalyzedTree;
pub use typecheck::{format_type, substitute_generics_in_typenode};
pub use workspace::{
    analyze_entry, analyze_entry_with_options, LoadError, LoadedModule, ModuleLoader, Workspace,
    WorkspaceDiagnostic, WorkspaceDiagnosticItem, WorkspaceTree,
};

use relon_parser::Node;
use std::collections::{HashMap, HashSet};

/// True when `root` declares a bare `#relaxed` or `#unstrict`
/// directive on its directive stack. Either spelling opts the module
/// out of strict inference (the analyzer's default). Used by
/// [`analyze_with_options`] to disable strict mode whenever the root
/// opts out, regardless of the workspace flag the caller forwarded.
pub(crate) fn has_relaxed_directive(root: &Node) -> bool {
    root.directives
        .iter()
        .any(|d| d.name == directive_names::RELAXED || d.name == directive_names::UNSTRICT)
}

/// v1.8 (C4 audit): walk every host-registered FnSignature and emit
/// the same `ExplicitAnyForbidden` / `BareGenericContainer`
/// diagnostics the user-source ban-walker fires. Without this a host
/// could ship `register_fn("foo", fn_of_signature("foo", &[Any], Any))`
/// and re-open the back-door v1.6 / v1.7 closed for user source.
///
/// Diagnostics carry `host fn '{name}' parameter '{param}'` /
/// `host fn '{name}' return type` / `host fn '{name}' variadic
/// tail` as context so the operator knows which host integration to
/// fix.
fn audit_host_fn_signatures(tree: &mut AnalyzedTree) {
    let sigs: Vec<(String, FnSignature)> = tree
        .host_fn_signatures
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for (fn_name, sig) in sigs {
        for param in &sig.params {
            let context = format!("host fn '{}' parameter '{}'", fn_name, param.name);
            ban_unsafe_types::scan_typenode_for_any(&param.ty, &context, &mut tree.diagnostics);
        }
        let ret_context = format!("host fn '{}' return type", fn_name);
        ban_unsafe_types::scan_typenode_for_any(
            &sig.return_type,
            &ret_context,
            &mut tree.diagnostics,
        );
        if let Some(tail) = &sig.variadic_tail {
            let tail_context = format!("host fn '{}' variadic tail", fn_name);
            ban_unsafe_types::scan_typenode_for_any(tail, &tail_context, &mut tree.diagnostics);
        }
    }
}

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
/// with their actual `Capabilities` bit grants.
pub fn analyze_with_options(root: &Node, options: &AnalyzeOptions) -> AnalyzedTree {
    let mut tree = AnalyzedTree::new();
    tree.host_fn_names = options.host_fn_names.clone();
    tree.host_fn_signatures = options.host_fn_signatures.clone();
    tree.host_fn_gates = options.host_fn_gates.clone();
    tree.caps = options.caps.clone();
    // v1.8 (C4 audit): every host-supplied signature is part of the
    // language surface — its parameter / return / variadic types are
    // visible to user source through stdlib-style resolution. Run the
    // same `Any` / bare-generic ban over them so a host that shipped
    // `register_fn` with `Any`-typed params can't silently re-open
    // the v1.6 / v1.7 back-doors. Diagnostics carry a `host fn`
    // context so the user can pinpoint which host integration is
    // misconfigured.
    audit_host_fn_signatures(&mut tree);
    // Strict is the default; the source can opt out via `#relaxed` /
    // `#unstrict`. The caller-supplied `strict_mode` flag is the
    // workspace-level decision (contagion of the entry's mode to
    // every reachable import); the per-file directive is the local
    // override. The AND means a strict workspace can be overridden
    // by a per-file opt-out (the contagion rule preserves this — a
    // relaxed entry stamps `strict_mode: false` on every import).
    tree.strict_mode = options.strict_mode && !has_relaxed_directive(root);
    // Schema-rooted decision 21' (core.relon carrier): install the
    // built-in `String` / `List<T>` / `Dict<K, V>` / `Iter<T>` method
    // tables before any user-source pass. Every subsequent collector
    // / checker sees built-in methods alongside user-declared ones —
    // that uniformity is what lets `s.upper()` dispatch through the
    // same path as `user_value.user_method()` with zero `#extend
    // String with { ... }` boilerplate.
    core_schemas::inject_core_schemas(&mut tree);
    schema::collect_schemas(root, &mut tree);
    // Root-level `#schema A Body` directives must run after
    // `collect_schemas` so the dual-declaration collision check has the
    // field-form table to consult.
    root_schemas::collect_root_schemas(root, &mut tree);
    // Schema-rooted Phase B: contribute `#extend X with { ... }` method
    // tables onto every schema declared above. Must run after both
    // schema collection passes so the in-scope name set is complete.
    extend::collect_extends(root, &mut tree);
    // Detect intra-block duplicate method names that survived the
    // per-pass conflict checks above (e.g. two methods of the same
    // name declared inside a single `with { ... }` block).
    extend::check_method_uniqueness(&mut tree);
    // Schema-rooted §J follow-up: warn when a method's generic
    // parameter shadows one of its owning schema's. The substitution
    // path treats the two names as the same binding key, so the
    // method body can't distinguish them. Pure warning — does not
    // gate evaluation.
    extend::check_method_generic_shadowing(&mut tree);
    // Schema-rooted Phase C.3: `#derive C` witness shape checking.
    // Must run after duplicate-name detection so a duplicate that's
    // also a witness gets the single `MethodNameConflict` instead of
    // a witness-shape diagnostic on every clone.
    constraints::check_derive_witnesses(&mut tree);
    // Schema-rooted Phase C.4: auto-derive Equatable / JsonProjectable
    // onto every user schema that hasn't opted out via
    // `#no_auto_derive` and doesn't already declare the witness
    // method. Must run *before* `build_method_signature_table` so the
    // synthesized methods land in `method_signatures` too.
    constraints::auto_derive_schemas(&mut tree);
    // Synthesize one `FnSignature` per method into the lookup table
    // consumed by `resolve_call_signature` and the evaluator.
    extend::build_method_signature_table(&mut tree);
    // Stage 2.8: schema field types must be either a builtin / prelude
    // name or a declared schema. Runs after both schema-collecting
    // passes so the known-name set is fully populated.
    schema::check_schema_field_types(&mut tree);
    main_sig::collect_main(root, &mut tree);
    // `collect_imports` must run before `resolve_references` so the
    // reference walker can detect cross-module heads (`#import alias`
    // → `alias.x`) and queue them for the workspace post-pass.
    // Resolution still works the same for any single-file source —
    // imports just stays empty.
    modules::collect_imports(root, &mut tree);
    resolve::resolve_references(root, &mut tree);
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
///
/// `strict_mode` defaults to `true`: every value must be statically
/// inferable, and sites the analyzer can't classify (uninferrable
/// spread sources, dynamic keys without a `<T>` hint, untyped closure
/// parameters, native fns with no signature, …) surface as errors.
/// Files can opt out by writing `#relaxed` (or `#unstrict`) at the
/// top, in which case those positions stay silent and the runtime
/// type-checks them on the way through.
///
/// The mode is *contagious* across `#import`s — the workspace pass
/// propagates the entry's mode to every reachable module so a relaxed
/// entry doesn't accidentally tighten because it imports a strict
/// library (or vice versa).
#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    /// Names registered with the host's `Context::functions`. Empty by
    /// default — hosts that want their custom fn names recognized must
    /// populate this from the keys of `Context::functions` directly.
    pub host_fn_names: HashSet<String>,
    /// Stage 3.4: signatures the host has declared for its native fns.
    /// When supplied, the FnCall checker validates arity / arg types
    /// against these signatures (same machinery the stdlib table
    /// drives). Names without a signature continue to participate only
    /// in the `host_fn_names` allowlist (silent on FnCall checking).
    pub host_fn_signatures: HashMap<String, FnSignature>,
    /// Stage 4: capability requirements declared by each registered
    /// native fn (mirror of the evaluator's `NativeFnGate` table). A
    /// missing entry means the fn isn't registered, or carries the
    /// empty gate (e.g. via `register_pure_fn`) — the static check
    /// stays silent on those. Hosts populate this from the `gate`
    /// argument they passed to `register_fn(name, gate, fn)`.
    pub host_fn_gates: HashMap<String, cap::NativeFnGate>,
    /// Stage 4: the host's actual capability grant (mirror of the
    /// evaluator's `Capabilities`). Used by the static reachability
    /// check to decide whether a gated fn would be denied at runtime.
    /// Defaults to zero-trust — same as the evaluator default.
    pub caps: cap::Capabilities,
    /// `true` (the default) enables strict inference: every value
    /// must have a statically inferable type. Sites that can't be
    /// classified produce error-severity diagnostics describing what
    /// couldn't be inferred. The per-file `#relaxed` / `#unstrict`
    /// directive overrides this to `false` for that module. The
    /// workspace pass propagates the entry module's mode to every
    /// reachable import so the two halves can't disagree.
    pub strict_mode: bool,
    /// v3++ b-2: when `true`, every `#import` whose path looks remote
    /// (`https://`, `http://`) must carry an inline integrity pin
    /// (`sha256:"..."`). Missing pins surface as
    /// [`crate::WorkspaceDiagnostic::ImportHashRequired`] before the
    /// loader is given a chance to fetch. Local-path imports are
    /// unaffected — the supply-chain risk model targets the network.
    /// Default `false` preserves the v3+ a-3 behavior.
    pub require_hash: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            host_fn_names: HashSet::new(),
            host_fn_signatures: HashMap::new(),
            host_fn_gates: HashMap::new(),
            caps: cap::Capabilities::default(),
            strict_mode: true,
            require_hash: false,
        }
    }
}
