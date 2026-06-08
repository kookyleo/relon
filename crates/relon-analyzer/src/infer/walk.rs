//! Inference sub-module: path-walking machinery.
//!
//! Three flavors of walker live here:
//!
//! * `path_segments` — name-only view of a path, stopping at the first
//!   non-String segment. Used by diagnostics that report a printable
//!   dotted path (`a.b.c`).
//! * `walk_segments` — name-or-index view that also captures `Index`
//!   segments (`pair.0`). Internal companion of [`walk_path`].
//! * `walk_path` — the full inference-time tail walker. Starts from
//!   the type returned by `scope.lookup(path[0])`, descends one
//!   segment at a time honoring `Schema(_)` field tables, `Dict<V>`
//!   value-type lifts, `Optional(_)` strip, `Tuple(_)` positional
//!   access, and the v1.4 alias-prefix shortcut for cross-module
//!   value references. Returns a [`PathTailOutcome`] so callers can
//!   distinguish fully-resolved walks from `UnknownHead` / `UnknownStep`.
//!
//! The two cross-cutting helpers `schema_generic_params` and
//! `qualify_type_node_for_alias` are kept here because they're only
//! consumed by `walk_path` — substituting the running schema's
//! generic params and rewriting bare imported-schema names into
//! their qualified form.

use super::{infer_from_type_node_with_imports, is_known_builtin_alt, InferredType, TypeScope};
use relon_parser::{TokenKey, TypeNode};
use std::collections::HashMap;

pub(crate) fn path_segments(path: &[TokenKey]) -> Vec<String> {
    let mut out = Vec::with_capacity(path.len());
    for seg in path {
        match seg {
            TokenKey::String(s, _, _) => out.push(s.clone()),
            TokenKey::Index(i, _) => out.push(i.to_string()),
            _ => break,
        }
    }
    out
}

/// v1.8 tuple-position access: walker-friendly view of a path that
/// preserves Index segments alongside named ones. Stops at the
/// first segment we can't statically classify (`Dynamic` /
/// `Spread`).
#[derive(Debug, Clone)]
enum WalkSeg {
    Name(String),
    Index(usize),
}

fn walk_segments(path: &[TokenKey]) -> Vec<WalkSeg> {
    let mut out = Vec::with_capacity(path.len());
    for seg in path {
        match seg {
            TokenKey::String(s, _, _) => out.push(WalkSeg::Name(s.clone())),
            TokenKey::Index(i, _) => out.push(WalkSeg::Index(*i)),
            _ => break,
        }
    }
    out
}

/// Schema-rooted §J follow-up (generics): look up `schema_name`'s
/// declared generic-parameter names. Tries (in order):
/// - the importer's local `tree.schemas` (dict-form schemas)
/// - the importer's `tree.root_schemas` (directive-form schemas)
/// - the workspace import index's `imported_schema_generics` (for
///   schemas reached through `#import alias from ...` /
///   `#import * from ...` / `#import { ... } from ...`)
///
/// Returns an empty vec if the schema isn't generic or can't be
/// found at all (conservative — callers treat missing params as "no
/// substitution to apply").
fn schema_generic_params(scope: &TypeScope, schema_name: &str) -> Vec<String> {
    if let Some(tree) = scope.tree {
        for def in tree.schemas.values() {
            if def.name.as_deref() == Some(schema_name) {
                return def.generics.clone();
            }
        }
        for d in &tree.root_schemas {
            if d.name == schema_name {
                return d.generics.clone();
            }
        }
        if let Some(idx) = tree.workspace_import_index.as_ref() {
            if let Some(params) = idx.imported_schema_generics.get(schema_name) {
                return params.clone();
            }
        }
    }
    Vec::new()
}

fn schema_tuple_elements(scope: &TypeScope, schema_name: &str) -> Option<Vec<TypeNode>> {
    let tree = scope.tree?;
    for def in tree.schemas.values() {
        if def.name.as_deref() == Some(schema_name) {
            return def.tuple_elements.clone();
        }
    }
    if let Some(idx) = tree.workspace_import_index.as_ref() {
        if let Some(elements) = idx.imported_tuple_schemas.get(schema_name) {
            return Some(elements.clone());
        }
    }
    None
}

/// Schema-rooted §J follow-up: rewrite a single-segment user-schema
/// `TypeNode` so its head carries the importer's alias prefix —
/// `User` (recorded inside `lib_with_value.relon`) becomes
/// `lib.User` when read through `#import lib`. Builtin / prelude
/// names are left untouched so primitive types and generic
/// containers don't sprout phantom alias prefixes.
///
/// This is what makes `aliased_values[alias][field]` lifts land on
/// the same qualified schema key (`alias.Name`) that
/// `build_schema_index` already merged from `imported_schemas`.
/// Without it, the bare `User` would lift to `Schema("User")`,
/// `walk_path`'s mid-step schema lookup would miss the importer's
/// `lib.User` entry, and the rest of the chain would surface as
/// `UnknownStep`.
///
/// Generics: when the value's declared type carries type arguments
/// (`Container<T> c: ...`), we recurse into each generic so a
/// nested user schema also gets the alias prefix —
/// `Container<User>` becomes `pkg.Container<pkg.User>`. Builtins
/// (`Int`, `List<…>`, `Closure<…>`, …) and the `Self` placeholder
/// stay un-qualified because their identity doesn't change across
/// module boundaries. Multi-segment heads (already qualified, or
/// hostile) are passed through unchanged on the assumption the
/// author wrote the prefix deliberately.
fn qualify_type_node_for_alias(hint: &TypeNode, alias: &str) -> TypeNode {
    let mut out = hint.clone();
    // Recurse into generics first so the result is consistent
    // regardless of whether the head also needs prefixing.
    out.generics = hint
        .generics
        .iter()
        .map(|g| qualify_type_node_for_alias(g, alias))
        .collect();
    // Only single-segment heads are eligible for prefix rewriting.
    // Multi-segment paths were already qualified by their author (or
    // are some other shape we shouldn't touch here).
    if hint.path.len() == 1 {
        let head = hint.path[0].as_str();
        // `Self` is a binder placeholder used in schema methods
        // (`eq(other: Self) -> Bool`). It refers to the enclosing
        // schema, not a free name, so it must not gain an alias
        // prefix. Builtins (`Int` / `List` / `Dict` / `Closure` /
        // …) are language-level and stable across modules.
        if head != "Self" && !is_known_builtin_alt(head) {
            out.path = vec![alias.to_string(), hint.path[0].clone()];
        }
    }
    out
}

/// Outcome of walking the tail of a `Variable` / `Reference` path under
/// the inference engine. Lets callers distinguish "fully resolved" from
/// "head was found but a middle segment is opaque" — the latter being
/// the precise shape strict mode wants to flag.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PathTailOutcome {
    /// Walk completed; the final type is `ty`. `ty` may itself be
    /// `Any` when the user explicitly typed something as `Any`, but
    /// every intermediate hop succeeded.
    Resolved(InferredType),
    /// Head resolved but a later segment couldn't be classified (e.g.
    /// the schema isn't visible, or the running type doesn't admit
    /// nested fields). `at_segment` is the 0-based index into `path`
    /// of the failing segment; `running_name` is the human-readable
    /// type-name we had at the point of failure.
    UnknownStep {
        at_segment: usize,
        running_name: String,
    },
    /// Path head itself isn't visible in the active scope. Strict mode
    /// turns this into `UnknownReferenceType`; non-strict callers fall
    /// back to `Any`.
    UnknownHead,
}

/// Walk an arbitrary dotted path under `scope`, starting from the type
/// returned by `scope.lookup(path[0])` and descending one segment at a
/// time:
///
/// * `Schema(name)` → segment must name a declared field of that
///   schema (looked up in `scope.schemas`); the walk continues with
///   the field's declared type.
/// * `Dict(value_ty)` → every key has the same value type, so the
///   segment is structurally fine and the walk continues with
///   `value_ty`.
/// * `Optional(inner)` → strip the `?` wrapper and try again; matches
///   the runtime's `T? . x` semantics.
/// * `Schema` referring to a name not in the schema index → soft stop
///   (`UnknownStep`), so strict mode reports the user-visible reason.
/// * Anything else (`Int`, `String`, `List<…>`, …) → `UnknownStep`,
///   because a non-schema, non-dict head can't have nested fields.
///
/// Recursion depth is naturally bounded by `path.len()` (each iteration
/// strips one segment), so the walk terminates regardless of schema
/// shape.
pub(crate) fn walk_path(path: &[TokenKey], scope: &TypeScope) -> PathTailOutcome {
    let segs = walk_segments(path);
    let Some(WalkSeg::Name(head)) = segs.first() else {
        // Path empty, or starts with an index — not a valid path
        // shape (the parser only produces Index segments after a
        // String head).
        return PathTailOutcome::UnknownHead;
    };
    // Schema-rooted §J follow-up: 2-segment alias-prefixed value
    // lookup. When `head` names a known import alias and the next
    // segment matches a value field exported by that alias's module,
    // synthesize the walking-current type from `aliased_values[alias]
    // [field]` and skip both segments before descending. Without this,
    // `pkg.alice.region` would stop at `scope.lookup("pkg")` (an alias
    // is not a regular binding) and `walk_path` would return
    // `UnknownHead`, silently leaking the rest of the chain to `Any`
    // through `infer_path_inferred`.
    //
    // The value's type-hint TypeNode (`User`) is recorded under the
    // exporter's namespace — bare schema names that the importer's
    // own `tree.schemas` doesn't carry. Before lifting we splice the
    // alias prefix onto single-segment schema paths so the result
    // lands on the same qualified key (`lib.User`) that
    // `build_schema_index` merged from `imported_schemas`. Builtin
    // primitives stay un-qualified.
    let mut start_offset = 0usize;
    let mut current: InferredType;
    // Schema-rooted §J follow-up (generics): substitution context for
    // the current schema's generic parameters. Populated when we
    // enter a generic-instantiated schema (via the alias-value
    // shortcut below or by descending into a field whose declared
    // type is `Container<...>`). Empty for non-generic schemas and
    // for non-schema running types.
    let mut current_subst: HashMap<String, TypeNode> = HashMap::new();
    // Schema-rooted §J follow-up: alias prefix under which the
    // current schema lives, when reached through a cross-module
    // import. Bare schema names in field types of an imported schema
    // refer to siblings in the *exporter's* namespace, so we need to
    // re-qualify them before lifting (and before lookup). Empty
    // string means "no namespace re-qualification needed".
    let mut current_namespace: Option<String> = None;
    let alias_value_resolved = if segs.len() >= 2 {
        if let WalkSeg::Name(field) = &segs[1] {
            scope
                .tree
                .and_then(|t| t.workspace_import_index.as_ref())
                .and_then(|idx| idx.aliased_values.get(head))
                .and_then(|values| values.get(field))
                .map(|hint| {
                    // Qualify the lib-side hint so a bare `Container`
                    // becomes `alias.Container` — the qualified key
                    // `build_schema_index` already uses for imported
                    // schemas. Recurse into generics so a nested
                    // user schema (`Container<User>`) also gains the
                    // alias prefix; builtins stay untouched.
                    let qualified = qualify_type_node_for_alias(hint, head);
                    let lifted = infer_from_type_node_with_imports(
                        &qualified,
                        scope.tree.and_then(|t| t.workspace_import_index.as_ref()),
                    );
                    (lifted, qualified)
                })
        } else {
            None
        }
    } else {
        None
    };
    if let Some((ty, qualified_hint)) = alias_value_resolved {
        current = ty;
        start_offset = 1;
        // Schema-rooted §J follow-up (generics): record the
        // schema's generic-arg bindings so the very next `.field`
        // step can substitute `T → ConcreteArg` when reading the
        // schema's declared field type.
        if let InferredType::Schema(ref schema_name) = current {
            current_namespace = Some(head.clone());
            let params = schema_generic_params(scope, schema_name);
            for (i, p) in params.iter().enumerate() {
                if let Some(arg) = qualified_hint.generics.get(i) {
                    current_subst.insert(p.clone(), arg.clone());
                }
            }
        }
    } else {
        let Some(looked_up) = scope.lookup(head) else {
            return PathTailOutcome::UnknownHead;
        };
        current = looked_up;
    }
    for (offset, seg) in segs[1 + start_offset..].iter().enumerate() {
        // Re-base offset so `at_segment` indices reported in
        // `UnknownStep` line up with the *original* `path` —
        // callers (strict-mode diagnostics) read `at_segment` to
        // pluck the failing source name, which is unaffected by the
        // alias-prefix shortcut. After the shortcut, the loop body
        // is checking `segs[1 + start_offset + offset]`, so the
        // original-index is `1 + start_offset + offset`.
        let at_segment = 1 + start_offset + offset;
        // Strip Optional wrappers before stepping, so `Maybe<T> . x`
        // is checked against `T`'s field set.
        if let InferredType::Optional(inner) = current {
            current = *inner;
        }
        match (current.clone(), seg) {
            (InferredType::Any, _) => {
                // After v1.6 ban-`Any` and v1.7 ban-bare-generic, the
                // only path-head that can still land here is a closure
                // parameter without a `type_hint` under non-strict
                // mode (strict raises `ClosureParamTypeMissing`
                // and never reaches the walker). Propagate `Any` so
                // non-strict callers continue to defer to runtime.
                return PathTailOutcome::Resolved(InferredType::Any);
            }
            (InferredType::Schema(schema_name), WalkSeg::Name(name)) => {
                let Some(schemas) = scope.schemas else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: schema_name,
                    };
                };
                let Some(fields) = schemas.get(&schema_name) else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: schema_name,
                    };
                };
                let Some(field_ty) = fields.get(name) else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: schema_name,
                    };
                };
                // Schema-rooted §J follow-up (generics): apply the
                // running substitution (`{T → Int}`) to the field's
                // declared type before lifting. This is what turns
                // `T value: *` into `Int value: *` when the running
                // schema was reached as `Container<Int>`.
                let substituted = if current_subst.is_empty() {
                    field_ty.clone()
                } else {
                    crate::typecheck::substitute_generics_in_typenode(field_ty, &current_subst)
                };
                // Schema-rooted §J follow-up: if the running schema
                // lives in an imported module's namespace, bare
                // sibling-schema references in its field types
                // mean "schemas in that same exporter's namespace",
                // so re-qualify before lifting. Builtins and binder
                // placeholders (`Self`, the schema's own generic
                // params) stay untouched.
                let renamespaced = if let Some(ns) = current_namespace.as_deref() {
                    qualify_type_node_for_alias(&substituted, ns)
                } else {
                    substituted
                };
                current = infer_from_type_node_with_imports(
                    &renamespaced,
                    scope.tree.and_then(|t| t.workspace_import_index.as_ref()),
                );
                // After the step, rebuild the substitution map and
                // namespace for the *new* running schema. A
                // non-schema running type (e.g. `Int`, `List<…>`)
                // resets both because there are no generic
                // parameters to bind further.
                current_subst.clear();
                if let InferredType::Schema(ref new_schema) = current {
                    // If `new_schema` is qualified (`alias.Name`),
                    // record the alias as the new namespace for
                    // subsequent descents.
                    if let Some((ns, _)) = new_schema.split_once('.') {
                        current_namespace = Some(ns.to_string());
                    }
                    // Build a fresh substitution from the field
                    // type's generic args against the new schema's
                    // declared parameters (post-substitution +
                    // re-namespacing, so a `Container<T>` field
                    // already has T bound to the parent's args).
                    let new_params = schema_generic_params(scope, new_schema);
                    for (i, p) in new_params.iter().enumerate() {
                        if let Some(arg) = renamespaced.generics.get(i) {
                            current_subst.insert(p.clone(), arg.clone());
                        }
                    }
                } else {
                    current_namespace = None;
                }
            }
            (InferredType::Schema(schema_name), WalkSeg::Index(i)) => {
                let Some(elements) = schema_tuple_elements(scope, &schema_name) else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: schema_name,
                    };
                };
                let arity = elements.len();
                let Some(element_ty) = elements.get(*i) else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: format!("{schema_name} tuple of arity {arity}"),
                    };
                };
                let substituted = if current_subst.is_empty() {
                    element_ty.clone()
                } else {
                    crate::typecheck::substitute_generics_in_typenode(element_ty, &current_subst)
                };
                let renamespaced = if let Some(ns) = current_namespace.as_deref() {
                    qualify_type_node_for_alias(&substituted, ns)
                } else {
                    substituted
                };
                current = infer_from_type_node_with_imports(
                    &renamespaced,
                    scope.tree.and_then(|t| t.workspace_import_index.as_ref()),
                );
                current_subst.clear();
                current_namespace = None;
            }
            (InferredType::Dict(value_ty), WalkSeg::Name(_)) => {
                current = *value_ty;
            }
            // v1.8: positional access on a Tuple. `pair.0` /
            // `pair.1` produce the i-th element's type; out-of-
            // range indices surface as `UnknownStep` so strict mode
            // reports the user-visible reason.
            (InferredType::Tuple(elems), WalkSeg::Index(i)) => {
                let arity = elems.len();
                if let Some(elem) = elems.into_iter().nth(*i) {
                    current = elem;
                } else {
                    return PathTailOutcome::UnknownStep {
                        at_segment,
                        running_name: format!("Tuple of arity {arity}"),
                    };
                }
            }
            (InferredType::Tuple(elems), WalkSeg::Name(_)) => {
                // Tuples are positional — stepping by name is a hard
                // failure. (Use `pair.0` instead of `pair.first`.)
                return PathTailOutcome::UnknownStep {
                    at_segment,
                    running_name: InferredType::Tuple(elems).name(),
                };
            }
            // v1.8: positional access on a List yields its element
            // type. Out-of-range indices can't be statically rejected
            // (the literal length isn't tracked here), so we accept
            // and let runtime own the bounds check.
            (InferredType::List(elem), WalkSeg::Index(_)) => {
                current = *elem;
            }
            (other, _) => {
                // Int/String/Bool/Closure/Variant/etc. don't have
                // user-visible nested fields, and a tuple wasn't
                // matched by the more specific arms above.
                return PathTailOutcome::UnknownStep {
                    at_segment,
                    running_name: other.name(),
                };
            }
        }
    }
    PathTailOutcome::Resolved(current)
}

/// Convenience wrapper for callers that don't care *why* the walk
/// stopped — only "what type did we end up with, if any". `UnknownStep`
/// and `UnknownHead` both collapse to `None` so the caller falls back
/// to whatever its own "uninferrable" branch does (typically `Any`).
pub(super) fn infer_path_inferred(path: &[TokenKey], scope: &TypeScope) -> Option<InferredType> {
    match walk_path(path, scope) {
        PathTailOutcome::Resolved(t) => Some(t),
        PathTailOutcome::UnknownStep { .. } => Some(InferredType::Any),
        PathTailOutcome::UnknownHead => None,
    }
}
