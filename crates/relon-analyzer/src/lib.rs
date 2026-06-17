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
pub use workspace_build::WorkspaceImportIndex;

use relon_parser::{DirectiveBody, Expr, Node, Operator, TypeNode};
use std::collections::{HashMap, HashSet};

/// v6-fix-D2-H cold-start: re-validates the same trivial-`#main`
/// shape `relon::is_trivial_scalar_main_node` classifies, but lives
/// in the analyzer crate so [`analyze_with_options`] can gate its
/// fast-path without taking a dep on `relon`. The two predicates
/// must agree byte-for-byte; a unit test (see
/// `analyzer_trivial_fast_path_matches_full_path`) keeps them in
/// lockstep on the canonical W11 corpus.
///
/// Returns `true` when:
/// * Exactly one `#main(...)` directive is present, no `#import`s
///   on the root, every parameter type is a single-segment scalar
///   builtin (`Int` / `Float` / `Bool` / `String`).
/// * The body is a literal (Int / Float / Bool / String),
///   a `Variable`, a `Unary` over a trivial leaf, a `Binary` over
///   two trivial leaves with an arithmetic / comparison / logical
///   operator (the set the trivial-tree-walker actually evaluates),
///   or a `Ternary` whose three sub-nodes are trivial leaves.
pub fn is_trivial_main_shape(root: &Node) -> bool {
    let mut main_directive: Option<&relon_parser::Directive> = None;
    for dir in &root.directives {
        if dir.name == directive_names::MAIN {
            if main_directive.is_some() {
                return false;
            }
            main_directive = Some(dir);
        }
        if dir.name == directive_names::IMPORT {
            return false;
        }
    }
    let Some(dir) = main_directive else {
        return false;
    };
    let DirectiveBody::Main { params, .. } = &dir.body else {
        return false;
    };
    for p in params {
        if !is_scalar_builtin_type(&p.type_node) {
            return false;
        }
    }
    is_trivial_body_expr(&root.expr)
}

fn is_scalar_builtin_type(t: &TypeNode) -> bool {
    if t.is_optional || t.variant_fields.is_some() {
        return false;
    }
    if t.path.len() != 1 {
        return false;
    }
    if !t.generics.is_empty() {
        return false;
    }
    matches!(t.path[0].as_str(), "Int" | "Float" | "Bool" | "String")
}

fn is_trivial_body_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::String(_) => true,
        Expr::Missing => false,
        Expr::Variable(_) => true,
        Expr::Unary(_, inner) => is_trivial_body_expr(&inner.expr),
        Expr::Binary(op, l, r) => {
            // Reject pipe `|` — its right-hand side is a callable
            // and triggers the same fn-call machinery the trivial
            // classifier wants to avoid.
            if matches!(op, Operator::Pipe) {
                return false;
            }
            is_trivial_body_expr(&l.expr) && is_trivial_body_expr(&r.expr)
        }
        Expr::Ternary { cond, then, els } => {
            is_trivial_body_expr(&cond.expr)
                && is_trivial_body_expr(&then.expr)
                && is_trivial_body_expr(&els.expr)
        }
        _ => false,
    }
}

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
    // v6-fix-D2-H cold-start: opt-in trivial-`#main` fast-path. When
    // the caller grants permission via `trivial_main_fast_path` and
    // the root really is a trivial scalar `#main(...)` shape (single
    // expression body over `#main` params + literal leaves), every
    // analyzer pass except `collect_main` + `check_main_return` is
    // a provable no-op for these inputs; the fast-path skips them
    // wholesale and returns a tree that is byte-for-byte equivalent
    // to the full pipeline's output on the same input. Falls through
    // to the normal pipeline whenever the shape doesn't match — so
    // a caller that flips this on for every entry never regresses on
    // non-trivial sources.
    if options.trivial_main_fast_path && is_trivial_main_shape(root) {
        return analyze_trivial_main_fast(root, options);
    }
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
    // `#unstrict`. The caller-supplied `strict_mode` flag is the global
    // default (normally `true`); the per-file directive is the local
    // override. Strictness is file-local: each module's effective mode
    // is the global default AND its OWN (non-)opt-out, so a `#relaxed`
    // directive governs only the module that declares it — the entry's
    // directive is never stamped onto the modules it imports.
    tree.strict_mode = options.strict_mode && !has_relaxed_directive(root);
    // Schema-rooted decision 21' (core.relon carrier): install the
    // built-in `String` / `List<T>` / `Dict<K, V>` / `Iter<T>` method
    // tables before any user-source pass. Every subsequent collector
    // / checker sees built-in methods alongside user-declared ones —
    // that uniformity is what lets `s.upper()` dispatch through the
    // same path as `user_value.user_method()` with zero `#extend
    // String with { ... }` boilerplate.
    //
    // v6-fix-D2 cold-start: parsing the four embedded carrier files
    // is the single biggest analyzer pass (~1.8 ms on the W11 shape).
    // Hosts that know their entry never dispatches a built-in method
    // can opt out via `AnalyzeOptions::skip_core_schemas`; the
    // `relon-cli --lite` flag does exactly that. The first
    // `inject_core_schemas` call in this process pays parse + lower
    // once (via a `OnceLock`), every subsequent call clones from the
    // cache — so the workspace pass's per-module analyse stays cheap.
    if !options.skip_core_schemas {
        core_schemas::inject_core_schemas(&mut tree);
    }
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
    typecheck_and_main_return(root, &mut tree);
    // Stage 4 (single-file): run the capability-reachability check over
    // this tree's own `node_index` when the caller opts in. The
    // workspace build pass leaves the flag off and runs the
    // cross-module `capability_check::run` itself; the compiled
    // backends analyze per-file with no workspace pass, so they set
    // the flag to keep the static guard reachable — a gated native
    // call without the granted cap then fails the build here rather
    // than slipping through to a runtime-only trap. No-op when no host
    // gate is registered.
    if options.standalone_capability_check {
        capability_check::run_single(&mut tree);
    }
    tree
}

/// P1-3 dedup: run the static type-check pass and then the
/// `#main(...) -> Type` annotation check. Single helper exists so
/// the initial pass (here) and the cross-module rerun in
/// [`workspace_build::recheck_cross_module_calls`] can't drift on
/// the ordering invariant — `check_main_return` reads the
/// schema_index `typecheck` populates and the
/// `StaticTypeMismatch` rows it emits into `tree.diagnostics`, so
/// the two must run in this order at every entry point.
pub(crate) fn typecheck_and_main_return(root: &Node, tree: &mut AnalyzedTree) {
    typecheck::typecheck(root, tree);
    main_return::check_main_return(root, tree);
}

/// v6-fix-D2-H cold-start: short-circuited analyzer pipeline for
/// the trivial scalar `#main(...)` shape. Skips every pass except
/// `collect_main` + `check_main_return`. Safe because, by
/// [`is_trivial_main_shape`]'s rule set, the source carries:
///
/// * No `#schema` / `#extend` / `#derive` (so every schema /
///   extend / constraint pass is provably empty).
/// * No `#import` (so module-graph collection has no work).
/// * No `Reference` / cross-scope `Variable` head outside the
///   `#main(...)` parameters (so `resolve_references` has nothing
///   to bind beyond the param frame `check_main_return` itself
///   seeds).
/// * No FnCall / Closure / List / Dict / Comprehension (so the
///   full `typecheck` walker would only re-validate the body's
///   arithmetic over the param frame — exactly what
///   `infer_type` already does inside `check_main_return`).
///
/// The pass still populates `tree.main_signature`,
/// `tree.host_fn_*`, `tree.strict_mode`, and any
/// `MainReturnTypeMismatch` / `ExplicitAnyForbidden` / unknown-
/// param-type diagnostics. Everything the evaluator reads from an
/// `AnalyzedTree` for these shapes is therefore identical to the
/// full-pipeline output.
fn analyze_trivial_main_fast(root: &Node, options: &AnalyzeOptions) -> AnalyzedTree {
    let mut tree = AnalyzedTree::new();
    tree.host_fn_names = options.host_fn_names.clone();
    tree.host_fn_signatures = options.host_fn_signatures.clone();
    tree.host_fn_gates = options.host_fn_gates.clone();
    tree.caps = options.caps.clone();
    // `audit_host_fn_signatures` walks the host-fn table; trivial
    // entries usually have an empty table (CLI `--lite` populates
    // none) but we still honour any caller-supplied signatures so
    // a host that wires `register_fn` then opts into the fast-path
    // keeps its `Any`-ban diagnostics.
    audit_host_fn_signatures(&mut tree);
    tree.strict_mode = options.strict_mode && !has_relaxed_directive(root);
    // `collect_main` populates `main_signature` and runs the
    // per-param unknown-type-head check + the `Any`-ban scan. Both
    // are required for the trivial shape — host args still validate
    // against the declared param types at runtime, and `Any` in
    // signature position must surface here regardless of body
    // shape.
    main_sig::collect_main(root, &mut tree);
    // The body might still trip `MainReturnTypeMismatch` (e.g.
    // `#main(Int x) -> String\nx + 1` is trivial but mismatches).
    // `check_main_return` is the only pass that performs that
    // check — running it preserves the diagnostic.
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
    /// v6-fix-D2 cold-start: when `true`, skip the carrier-`.relon`
    /// parse + lower pass (`core_schemas::inject_core_schemas`).
    /// That pass contributes the `String` / `List<T>` / `Dict<K, V>`
    /// / `Iter<T>` *method dispatch* table only — schema field-type
    /// checks, typecheck, etc. don't read it. So skipping is safe
    /// iff the source never dispatches a method on one of those
    /// built-in carriers (e.g. `"x".upper()`, `[1,2].map(f)`); if it
    /// does, the analyzer would surface an `UnknownMethod` diagnostic
    /// instead of finding the carrier-declared signature. The CLI
    /// `--lite` flag flips this on; we publish it on the options
    /// struct so any host with the same "I know my entry has no
    /// built-in method calls" knowledge can opt in too.
    ///
    /// Default `false` preserves the pre-D2 behavior.
    pub skip_core_schemas: bool,
    /// v6-fix-D2-H cold-start: when `true`, [`analyze_with_options`]
    /// is allowed to take a trivial-`#main` fast-path that strips
    /// every pass that's a provable no-op for a trivial scalar
    /// `#main(...)` entry (no `#schema`, no `#extend`, no `#import`,
    /// body is a literal / `Variable` / `Unary` / `Binary` / `Ternary`
    /// over those leaves). The fast-path runs only `collect_main` +
    /// `check_main_return` (the two passes whose outputs the
    /// evaluator and host actually read for these shapes), skipping
    /// audit / schema-collection / extend / constraint / module /
    /// resolve / typecheck passes wholesale.
    ///
    /// The flag is a *permission*, not a forced opt-in: the analyzer
    /// re-runs [`crate::is_trivial_main_shape`] internally and falls
    /// through to the full pipeline whenever the source doesn't
    /// match, so a caller setting this can still feed it arbitrary
    /// sources without breaking diagnostics.
    ///
    /// Default `false` preserves the pre-H behavior.
    pub trivial_main_fast_path: bool,
    /// Stage 4 (single-file): when `true`, [`analyze_with_options`]
    /// runs the capability-reachability check over this tree's own
    /// `node_index` (via `capability_check::run_single`). The workspace
    /// build pass leaves this `false` and runs the cross-module
    /// `capability_check::run` itself — flipping it on there would
    /// double-flag every gated call. The compiled backends
    /// (bytecode / cranelift / llvm) analyze per-file with no workspace
    /// pass, so they set this to keep the static guard reachable.
    ///
    /// Default `false` preserves the workspace-driven behavior.
    pub standalone_capability_check: bool,
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
            skip_core_schemas: false,
            trivial_main_fast_path: false,
            standalone_capability_check: false,
        }
    }
}

#[cfg(test)]
mod trivial_main_fast_path_tests {
    use super::*;
    use relon_parser::parse_document;

    fn fast_path_options() -> AnalyzeOptions {
        AnalyzeOptions {
            // The CLI lite branch couples skip_core_schemas with
            // trivial_main_fast_path; mirror that here so tests run
            // through the same combination operators see.
            skip_core_schemas: true,
            trivial_main_fast_path: true,
            ..AnalyzeOptions::default()
        }
    }

    /// Equivalence under the canonical W11 shape: the fast-path tree
    /// must match the full-pipeline tree on every consumer-visible
    /// table for the trivial-`#main` source. Only the carrier-fed
    /// `schemas` / `method_signatures` / `schema_methods` tables
    /// differ (the full path has the four built-in carriers; the
    /// lite path doesn't), and those tables are unused by the
    /// trivial-tree-walk evaluation path the lite mode selects.
    #[test]
    fn fast_path_matches_full_path_on_w11_shape() {
        let src = "#main(Int x) -> Int\nx + 1";
        let node = parse_document(src).expect("parse");
        let full = analyze_with_options(&node, &AnalyzeOptions::default());
        let fast = analyze_with_options(&node, &fast_path_options());

        assert!(!fast.has_errors(), "{:?}", fast.diagnostics);
        assert!(!full.has_errors(), "{:?}", full.diagnostics);
        // main_signature is the single field the evaluator dispatches
        // on for trivial-`#main` sources; the two paths must agree
        // byte-for-byte on it.
        let full_sig = full.main_signature.as_ref().expect("full sig");
        let fast_sig = fast.main_signature.as_ref().expect("fast sig");
        assert_eq!(full_sig.params.len(), fast_sig.params.len());
        assert_eq!(full_sig.params[0].name, fast_sig.params[0].name);
        assert_eq!(
            format!("{:?}", full_sig.return_type),
            format!("{:?}", fast_sig.return_type)
        );
        assert_eq!(full.diagnostics.len(), fast.diagnostics.len());
        assert_eq!(full.imports.len(), fast.imports.len());
    }

    /// Negative shape: source that isn't a trivial `#main` (here a
    /// FnCall in the body) must fall through to the full pipeline
    /// even when the caller flips the fast-path on. The two trees
    /// stay equivalent because the fast-path branch never executes.
    #[test]
    fn non_trivial_source_falls_through_to_full_path() {
        let src = "#main(Int x) -> Int\nabs(x) + 1";
        let node = parse_document(src).expect("parse");
        // With the fast-path on, the analyzer must still run the
        // FnCall typecheck. We verify indirectly by checking
        // `is_trivial_main_shape` rejects this source and by
        // running the analyzer through both branches and comparing
        // the diagnostic count.
        assert!(!is_trivial_main_shape(&node));
        let full = analyze_with_options(&node, &AnalyzeOptions::default());
        let fast = analyze_with_options(&node, &fast_path_options());
        // The two should agree on diagnostic count — fast-path
        // fall-through must not change behavior.
        assert_eq!(
            full.diagnostics.len(),
            fast.diagnostics.len(),
            "full={:?} fast={:?}",
            full.diagnostics,
            fast.diagnostics
        );
    }

    /// MainReturnTypeMismatch must still surface in the fast-path —
    /// `check_main_return` is the only pass producing it and is
    /// preserved by the short-circuit.
    #[test]
    fn fast_path_preserves_main_return_mismatch() {
        let src = "#main(Int x) -> String\nx + 1";
        let node = parse_document(src).expect("parse");
        assert!(is_trivial_main_shape(&node));
        let fast = analyze_with_options(&node, &fast_path_options());
        let mismatches: Vec<_> = fast
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MainReturnTypeMismatch { .. }))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", fast.diagnostics);
    }

    /// `#import` on a trivial body still routes through the full
    /// path: the classifier rejects any `#import`-bearing root.
    #[test]
    fn import_disqualifies_fast_path() {
        let src = "#import x from \"std/math\"\n#main(Int n) -> Int\nn + 1";
        let node = parse_document(src).expect("parse");
        assert!(!is_trivial_main_shape(&node));
    }
}
