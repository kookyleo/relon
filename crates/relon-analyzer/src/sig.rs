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
    pub params: Vec<FnParam>,
    pub return_type: TypeNode,
    /// When `Some`, the call may receive zero or more *additional*
    /// trailing arguments of this type after the fixed `params`. Used
    /// for `_dict_merge` and the `range` 1-or-2-arg form.
    pub variadic_tail: Option<TypeNode>,
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

/// Resolve `name` against the closure-signature side-table on `tree`,
/// the host-supplied signature map, and finally the stdlib table.
/// Returns `None` when nothing matches — callers treat that as "defer to
/// runtime", not as an error.
pub fn lookup_signature<'a>(
    name: &str,
    tree: &'a AnalyzedTree,
    host_sigs: &'a HashMap<String, FnSignature>,
) -> Option<FnSignature> {
    // 1. User closure declared as a dict field. Indexed by field name
    //    so a `FnCall` whose head matches a sibling closure picks up the
    //    declared param / return types. We override the synthetic
    //    `<closure#...>` name with the source-level field name so
    //    diagnostics read naturally.
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
    stdlib_signatures().get(name).cloned()
}
