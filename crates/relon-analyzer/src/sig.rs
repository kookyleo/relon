//! Static function signatures for analyzer-side FnCall checking.
//!
//! Stage 3 introduces three sources of signatures the analyzer can
//! consult when validating a `FnCall`:
//!
//! 1. User closures defined in source — extracted from the `Closure`
//!    AST node by the type-check walker.
//! 2. Host-registered native fns — supplied via
//!    [`crate::AnalyzeOptions::host_fn_signatures`].
//! 3. Hardcoded stdlib signatures — provided by
//!    [`crate::stdlib_signatures::stdlib_signatures`].
//!
//! The shape is intentionally minimal (just enough to drive arity / arg
//! type checks); generic instantiation, named-arg coverage, and cross-
//! module closure signatures are explicitly deferred to v1.1+.

use crate::stdlib_signatures::stdlib_signatures;
use crate::tree::AnalyzedTree;
use crate::workspace_build::WorkspaceImportIndex;
use relon_parser::TypeNode;
use std::collections::HashMap;

/// One formal parameter on a [`FnSignature`].
#[derive(Debug, Clone)]
pub struct FnParam {
    /// Source-level name. Currently informational — v1 only checks by
    /// position; named-arg lookup is deferred.
    pub name: String,
    /// Declared parameter type. `Any` widens the slot.
    pub ty: TypeNode,
    /// True when the parameter may be omitted (e.g. `ensure.int`'s
    /// trailing `message?`).
    pub optional: bool,
}

/// Static signature for a callable. Built once per stdlib name (lazy
/// `OnceLock`), once per user-source closure (during the type-check
/// walk), or once per host-registered fn (when the host populates
/// [`crate::AnalyzeOptions::host_fn_signatures`]).
#[derive(Debug, Clone)]
pub struct FnSignature {
    pub name: String,
    /// v1.1: ordered list of generic type parameter names declared by
    /// this signature (e.g. `["T", "U"]`). Empty for monomorphic fns.
    /// Occurrences of these names inside `params[i].ty`,
    /// `variadic_tail`, or `return_type` (as a single-segment
    /// zero-generic `TypeNode`) are placeholders, instantiated at the
    /// call site by [`instantiate`].
    pub generics: Vec<String>,
    pub params: Vec<FnParam>,
    pub return_type: TypeNode,
    /// When `Some`, the call may receive zero or more *additional*
    /// trailing arguments of this type after the fixed `params`. Used
    /// for `_dict_merge` and the `range` 1-or-2-arg form.
    pub variadic_tail: Option<TypeNode>,
}

impl FnSignature {
    /// True when `name` matches one of this signature's declared
    /// generic parameter names. Used by the unification / substitution
    /// helpers to distinguish `T` (placeholder) from `Int` (concrete).
    pub fn is_generic_param(&self, name: &str) -> bool {
        self.generics.iter().any(|g| g == name)
    }
}

/// v1.1: apply a binding map to every type slot in `sig` (params,
/// `variadic_tail`, `return_type`), producing an instantiated copy.
/// Unbound generic placeholders are left as-is; the caller may treat
/// such residual references as "couldn't pin down — fall back to Any"
/// at use sites.
pub fn instantiate(
    sig: &FnSignature,
    bindings: &std::collections::HashMap<String, TypeNode>,
) -> FnSignature {
    if bindings.is_empty() || sig.generics.is_empty() {
        return sig.clone();
    }
    let mut out = sig.clone();
    for p in &mut out.params {
        substitute_in_type_node(&mut p.ty, &sig.generics, bindings);
    }
    if let Some(tail) = out.variadic_tail.as_mut() {
        substitute_in_type_node(tail, &sig.generics, bindings);
    }
    substitute_in_type_node(&mut out.return_type, &sig.generics, bindings);
    out
}

/// In-place substitution. A `TypeNode` is treated as a placeholder
/// when its `path` is a single segment listed in `generics` and it
/// carries no nested generics of its own (mirrors the
/// runtime-evaluator schema substitution rule). Optionality flags are
/// preserved by ORing the placeholder's `is_optional` with the
/// replacement's.
///
/// Exposed `pub(crate)` so the closure-aware unification pass in
/// `crate::generics::collect_bindings` can reuse the same rule when
/// projecting partial bindings into a closure-arg's child scope.
pub(crate) fn substitute_in_type_node(
    t: &mut TypeNode,
    generics: &[String],
    bindings: &std::collections::HashMap<String, TypeNode>,
) {
    if t.path.len() == 1 && t.generics.is_empty() && generics.iter().any(|g| g == &t.path[0]) {
        if let Some(replacement) = bindings.get(&t.path[0]) {
            let was_optional = t.is_optional;
            let range = t.range;
            *t = replacement.clone();
            t.is_optional = t.is_optional || was_optional;
            t.range = range;
            return;
        }
        // Unbound placeholder: leave as-is (caller decides).
        return;
    }
    for inner in &mut t.generics {
        substitute_in_type_node(inner, generics, bindings);
    }
}

/// Build a single-segment `TypeNode` for a builtin name (`Int`, `Bool`,
/// `String`, …). Reused by both [`crate::stdlib_signatures`] and any
/// caller that wants to emit a synthetic signature programmatically.
pub fn type_node_simple(name: &str) -> TypeNode {
    TypeNode {
        path: vec![name.to_string()],
        generics: Vec::new(),
        is_optional: false,
        range: relon_parser::TokenRange::default(),
        variant_fields: None,
        doc_comment: None,
    }
}

/// Build a single-segment generic `TypeNode` (`List<Int>`, `Dict<String, Any>`, …).
pub fn type_node_generic(name: &str, args: Vec<TypeNode>) -> TypeNode {
    TypeNode {
        path: vec![name.to_string()],
        generics: args,
        is_optional: false,
        range: relon_parser::TokenRange::default(),
        variant_fields: None,
        doc_comment: None,
    }
}

/// Resolve `name` against (in order) the closure-signature side-table
/// on `tree`, the host-supplied signature map, the stdlib table, and
/// finally any cross-module closure signatures exposed via the
/// importer's `WorkspaceImportIndex` (v1.1). Returns `None` when
/// nothing matches — callers treat that as "defer to runtime", not as
/// an error.
///
/// The v1.1 cross-module hop uses `tree.workspace_import_index` when
/// the tree was produced by the workspace build pass; single-file
/// `analyze` calls have no import index and skip the hop.
pub fn lookup_signature<'a>(
    name: &str,
    tree: &'a AnalyzedTree,
    host_sigs: &'a HashMap<String, FnSignature>,
) -> Option<FnSignature> {
    lookup_signature_path(&[name.to_string()], tree, host_sigs)
}

/// Path-aware variant of [`lookup_signature`]. Single-segment paths
/// behave identically to the legacy entry point. Multi-segment paths
/// only resolve through the v1.1 cross-module index — `alias.method`
/// lookups against `aliased_closures`. Other multi-segment forms
/// (dict-literal sibling closures) stay handled by the type-check
/// walker's bespoke resolver because they need scope-stack context this
/// lookup doesn't carry.
pub fn lookup_signature_path<'a>(
    path: &[String],
    tree: &'a AnalyzedTree,
    host_sigs: &'a HashMap<String, FnSignature>,
) -> Option<FnSignature> {
    if path.is_empty() {
        return None;
    }
    if path.len() == 1 {
        let name = &path[0];
        // 1. User closure declared as a dict field. Indexed by field
        //    name so a `FnCall` whose head matches a sibling closure
        //    picks up the declared param / return types. We override
        //    the synthetic `<closure#...>` name with the source-level
        //    field name so diagnostics read naturally.
        if let Some(node_id) = tree.field_closure_index.get(name).copied() {
            if let Some(sig) = tree.closure_signatures.get(&node_id) {
                let mut renamed = sig.clone();
                renamed.name = name.to_string();
                return Some(renamed);
            }
        }
        // 2. Host fn signatures — populated from `AnalyzeOptions`.
        if let Some(sig) = host_sigs.get(name) {
            return Some(sig.clone());
        }
        // 3. Stdlib hardcoded table.
        if let Some(sig) = stdlib_signatures().get(name).cloned() {
            return Some(sig);
        }
        // 4. v1.1: cross-module imports (spread / destructure forms).
        if let Some(idx) = tree.workspace_import_index.as_ref() {
            return lookup_in_import_index_single(name, idx);
        }
        return None;
    }
    if path.len() == 2 {
        // Multi-segment: only the v1.1 alias.method form lives here.
        // Dict-literal sibling closures are resolved by the type-check
        // walker (it has the scope stack we don't).
        if let Some(idx) = tree.workspace_import_index.as_ref() {
            if let Some(methods) = idx.aliased_closures.get(&path[0]) {
                if let Some(sig) = methods.get(&path[1]) {
                    return Some(sig.clone());
                }
            }
        }
    }
    None
}

/// Single-segment v1.1 cross-module lookup: spread imports first, then
/// destructured imports. Spread wins on collision (last spread wins
/// internally, then beats destructured) — v1 keeps the rule simple;
/// shadowing diagnostics are explicitly out of scope for v1.1.
fn lookup_in_import_index_single(name: &str, idx: &WorkspaceImportIndex) -> Option<FnSignature> {
    if let Some(sig) = idx.spread_closures.get(name) {
        return Some(sig.clone());
    }
    if let Some(sig) = idx.destructured_closures.get(name) {
        return Some(sig.clone());
    }
    None
}
