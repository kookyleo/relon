//! Type-check sub-module: small helpers shared across the walker's
//! method groups.
//!
//! Two categories live here:
//!
//! * Pure free fns (`format_type`, `levenshtein`, `closest_variant`,
//!   `same_outer_container`, `required_and_max`, `extract_closure_signature`,
//!   `param_is_polymorphic`, `stdlib_registered_names`, `stdlib_names`).
//!   None of them touch the `Walker` struct's mutable state — they're
//!   referenced from every check group, so keeping them in one place
//!   avoids fan-out.
//! * Small `Walker` extension methods (`build_type_scope` /
//!   `build_type_scope_with_closure` / `is_known_fn` /
//!   `lookup_field_node` / `dynamic_save`). Each is < 30 LoC and is
//!   called from multiple unrelated method groups (binding check,
//!   strict mode, fn-call dispatch, …), so they live alongside the
//!   free helpers rather than being duplicated in every domain file.

use super::Walker;
use crate::infer::{InferredType, TypeScope};
use crate::sig::{type_node_simple, FnParam, FnSignature};
use crate::tree::AnalyzedTree;
use relon_parser::{ClosureParam, Node, TypeNode};
use std::collections::HashMap;
use std::sync::Arc;

/// Schema-rooted §J follow-up helper: classify a method param's
/// `TypeNode` as polymorphic (still a placeholder name) vs concrete
/// (`Int`, `String`, a user schema, an alias-qualified schema, …).
///
/// Polymorphic means **the param type is exactly one of the in-scope
/// generic names** with no further structure — `key: K` on a
/// constraint witness whose `K` hasn't been pinned. Such a param
/// can't be statically validated against an arg type because the
/// receiver supplies the concrete binding only at runtime; the
/// `check_index_dispatch` walker silently skips.
///
/// "In-scope" is the union of (a) the method's own `generics` list
/// (e.g. `map<U>` declares `U`) and (b) the owning schema's generics
/// (`List<T>` declares `T`, visible to every method body). The
/// shadow-warning emitted in Item 3 catches the name-collision case;
/// here we treat both name spaces as polymorphic for the purpose of
/// "is this param still unbound".
pub(super) fn param_is_polymorphic(
    ty: &relon_parser::TypeNode,
    method_generics: &[String],
    schema_name: &str,
    tree: &AnalyzedTree,
) -> bool {
    if ty.path.len() != 1 || !ty.generics.is_empty() {
        return false;
    }
    let head = &ty.path[0];
    if method_generics.iter().any(|g| g == head) {
        return true;
    }
    let schema_generics: Vec<String> = tree
        .schemas
        .values()
        .find(|def| def.name.as_deref() == Some(schema_name))
        .map(|def| def.generics.clone())
        .or_else(|| {
            tree.root_schemas
                .iter()
                .find(|d| d.name == schema_name)
                .map(|d| d.generics.clone())
        })
        .unwrap_or_default();
    schema_generics.iter().any(|g| g == head)
}

/// Return `(required_count, max_fixed_count)` for the given signature.
/// `required_count` is the number of leading non-optional params;
/// `max_fixed_count` is the total fixed-param count (including
/// trailing optionals). Variadic tail handling is layered on top by
/// the caller.
pub(super) fn required_and_max(sig: &FnSignature) -> (usize, usize) {
    let max = sig.params.len();
    // Optional params are tail-only by convention (validators), but we
    // count from the back to be safe — the first non-optional encountered
    // anchors `required`.
    let mut required = max;
    for p in sig.params.iter().rev() {
        if p.optional {
            required -= 1;
        } else {
            break;
        }
    }
    (required, max)
}

/// Stage 3.3: derive a [`FnSignature`] from the closure AST. Each
/// `ClosureParam` becomes an `FnParam` with `optional: false` (v1
/// doesn't model defaulted params); the return type comes from the
/// explicit `-> T` annotation when present, otherwise defaults to `Any`
/// because the body's inferred type may depend on values we don't see
/// during the closure-collection phase. The returned signature is
/// stored on `AnalyzedTree::closure_signatures` and consulted by
/// [`crate::sig::lookup_signature`] when a sibling callable is invoked.
pub(super) fn extract_closure_signature(
    closure_node: &Node,
    params: &[ClosureParam],
    return_type: &Option<TypeNode>,
    _body: &Node,
) -> FnSignature {
    let fn_params: Vec<FnParam> = params
        .iter()
        .map(|p| FnParam {
            name: p.name.clone(),
            ty: p
                .type_hint
                .clone()
                .unwrap_or_else(|| type_node_simple("Any")),
            optional: false,
        })
        .collect();
    let return_ty = return_type
        .clone()
        .unwrap_or_else(|| type_node_simple("Any"));
    FnSignature {
        // Closures are anonymous at the language level; the analyzer
        // names them by their `NodeId` so diagnostics referring back to
        // the original site still have an unambiguous handle.
        name: format!("<closure#{:?}>", closure_node.id),
        // v1 user closures don't declare generic parameters in source,
        // so the placeholder list stays empty.
        generics: Vec::new(),
        params: fn_params,
        return_type: return_ty,
        variadic_tail: None,
    }
}

/// True when `inferred` and `expected` agree on their outer
/// container shape (`List`/`Dict`) but disagree somewhere deeper.
/// Used by `check_typed_binding` to decide whether to defer to the
/// structural element walker for a more precise diagnostic location.
pub(super) fn same_outer_container(inferred: &InferredType, expected: &TypeNode) -> bool {
    if expected.path.len() != 1 {
        return false;
    }
    matches!(
        (inferred, expected.path[0].as_str()),
        (InferredType::List(_), "List")
            | (InferredType::Dict(_), "Dict")
            // v1.7: list literals infer as Tuple, but they are
            // structurally lists from the surface syntax. When the
            // declared slot is `List<T>`, route the value through
            // the per-element walker so each `xs[i]: ...` mismatch
            // shows up with its precise position rather than the
            // coarser `(Int, String, Int) vs List<Int>` outer
            // diagnostic. Same idea for tuple-typed slots.
            | (InferredType::Tuple(_), "List")
            | (InferredType::Tuple(_), "Tuple")
    )
}

/// Compact `TypeNode` formatter shared by every diagnostic /
/// runtime-error site that needs to render a declared type. `pub` so the
/// evaluator can drop its own duplicate (the two implementations were
/// byte-identical and drifted by accident before).
pub fn format_type(t: &TypeNode) -> String {
    let suffix = if t.is_optional { "?" } else { "" };
    let path = t.path.join(".");
    if t.generics.is_empty() {
        format!("{path}{suffix}")
    } else {
        let inner: Vec<String> = t.generics.iter().map(format_type).collect();
        format!("{path}<{}>{suffix}", inner.join(", "))
    }
}

/// Find the closest variant name (case-insensitive Levenshtein distance
/// up to 2) for a did-you-mean hint. Returns `None` when nothing's close
/// enough to suggest.
pub(super) fn closest_variant(target: &str, candidates: &[String]) -> Option<String> {
    let mut best: Option<(usize, &String)> = None;
    let target_lower = target.to_lowercase();
    for cand in candidates {
        let dist = levenshtein(&target_lower, &cand.to_lowercase());
        if dist <= 2 && best.is_none_or(|(d, _)| dist < d) {
            best = Some((dist, cand));
        }
    }
    best.map(|(_, s)| s.clone())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Names actually registered by the evaluator's stdlib (mirrors
/// `crates/relon-evaluator/src/stdlib.rs::register_to`). Used by the
/// closure free-variable check so well-known names don't false-positive
/// as `UnresolvedReference`. The list also includes module aliases
/// commonly bound via `#import std/<name>`; they aren't strictly stdlib
/// fn names but the typecheck pass can't tell whether a `Variable("string")`
/// was bound by an in-scope `#import` we haven't statically modeled, so
/// we keep them silent rather than spam the user.
/// Names actually registered with [`Context::register_fn`] in the
/// evaluator. Mirrors `crates/relon-evaluator/src/stdlib.rs::register_to`
/// — kept lockstep via the drift-defense test.
pub(super) fn stdlib_registered_names() -> &'static [&'static str] {
    &[
        "len",
        "_len",
        "range",
        "type",
        "_list_map",
        "_list_filter",
        "_list_reduce",
        "_list_contains",
        "_string_split",
        "_string_join",
        "_string_replace",
        "_string_upper",
        "_string_lower",
        "_string_contains",
        "_dict_merge",
        "_dict_keys",
        "_dict_values",
        "_dict_has_key",
        "_math_abs",
        "_math_max",
        "_math_min",
        "_math_clamp",
        "ensure.int",
        "ensure.string",
        "ensure.bool",
        "ensure.float",
        "ensure.list",
        "ensure.dict",
        "ensure.at_least",
        "ensure.at_most",
        "ensure.one_of",
        "ensure.required_fields",
        "ensure.requires",
        "ensure.fields_equal",
    ]
}

pub(super) fn stdlib_names() -> &'static std::collections::HashSet<&'static str> {
    use std::sync::OnceLock;
    static NAMES: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        // Module aliases conventionally introduced by `#import std/<name>`.
        // The user's source might use them as bare variables before the
        // import directive lands in `tree.imports`; we keep them silent
        // so the legacy "well-known" feel is preserved.
        let import_aliases = [
            "list", "dict", "string", "math", "is", "value", "abs", "min", "max", "sum", "format",
            "type_of",
        ];
        let mut set = std::collections::HashSet::new();
        // `ensure` itself is the head of dotted paths like `ensure.int`;
        // the analyzer only sees the head when it appears as a Variable.
        set.insert("ensure");
        for n in stdlib_registered_names()
            .iter()
            .chain(import_aliases.iter())
        {
            set.insert(*n);
        }
        set
    })
}

impl<'a> Walker<'a> {
    /// Build a read-only `TypeScope` snapshot anchored on the walker's
    /// current scope stack + schema index. Lent to inference helpers
    /// while the walker holds a mutable borrow on `self.tree`.
    pub(super) fn build_type_scope(&self) -> TypeScope<'_> {
        TypeScope {
            locals: HashMap::new(),
            parent_locals: Vec::new(),
            schemas: Some(&self.schema_index),
            frames: self.scope_stack.iter().collect(),
            tree: Some(self.tree),
            resolving: Vec::new(),
        }
    }

    /// True when `name` is a stdlib-or-host function the user can call
    /// without a sibling binding. Also covers imported / spread /
    /// destructured / aliased names from cross-module index so a
    /// `User` reference (alias form) doesn't false-flag.
    pub(super) fn is_known_fn(&self, name: &str) -> bool {
        if stdlib_names().contains(name) {
            return true;
        }
        if self.tree.host_fn_names.contains(name) {
            return true;
        }
        if let Some(idx) = self.tree.workspace_import_index.as_ref() {
            if idx.spread_closures.contains_key(name)
                || idx.destructured_closures.contains_key(name)
                // Spread / destructure schema names also live on the
                // import index as type names; surface them through the
                // same allowlist so a `User` reference (alias form)
                // doesn't false-flag.
                || idx.spread.contains(name)
                || idx.destructured.contains_key(name)
                || idx.aliased.contains_key(name)
                || idx.aliased_closures.contains_key(name)
            {
                return true;
            }
        }
        false
    }

    /// Look up `name` against the active scope chain and return the
    /// `Arc<Node>` it binds to (the target field's value node). Walks
    /// from innermost to outermost frame to mirror the resolver.
    pub(super) fn lookup_field_node(&self, name: &str) -> Option<Arc<relon_parser::Node>> {
        for frame in self.scope_stack.iter().rev() {
            if let Some(id) = frame.fields.get(name).copied() {
                return self.tree.node_index.get(&id).cloned();
            }
        }
        None
    }

    /// True if any frame on the active scope chain has a dynamic
    /// spread or a closure param matching `name`.
    pub(super) fn dynamic_save(&self, name: &str) -> bool {
        self.scope_stack
            .iter()
            .rev()
            .any(|frame| frame.might_dynamically_bind(name))
    }
}
