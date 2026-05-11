//! Schema-rooted Phase B: `#extend` directive collection.
//!
//! A `#extend X with { ... }` block contributes additional methods to a
//! schema named `X` that has already been declared elsewhere — either
//! as a `#schema X { ... }` in the same module / a transitively
//! imported one, or as a built-in type name (`String`, `Int`, `List`,
//! ...). The analyzer treats `#extend X` and `#schema X with { ... }`
//! as siblings: both end up in `tree.schema_methods["X"]`, in
//! source-discovery order.
//!
//! The pass is intentionally name-only: it does not attempt to bind
//! `Self` / `self` or type-check the bodies. That happens later, when
//! the type-check pass walks each method body with a `self : X`
//! environment.
//!
//! Conflict policy (decision 8 of `schema-rooted-model-2026-05-11.md`):
//! method names must be unique across the union of `with { ... }` blocks
//! visible to the current module. We emit `MethodNameConflict` for any
//! duplicate observed locally; cross-module conflicts that would
//! collide only along a particular import chain are caught by the
//! workspace pass.

use crate::diagnostic::{span_of, Diagnostic};
use crate::directive_names::EXTEND;
use crate::schema::{method_info_from_parser, SchemaMethodInfo};
use crate::sig::{type_node_simple, FnParam, FnSignature};
use crate::tree::AnalyzedTree;
use relon_parser::{DirectiveBody, Node, TokenRange};
use std::collections::HashMap;

/// Set of built-in type names that may be extended via `#extend`.
/// Keep in sync with the type-check side (see `typecheck::format_type`'s
/// inverse and the evaluator's value-tagging conventions). When users
/// `#extend` one of these, the schema_methods entry is created lazily
/// — there is no `SchemaDef` for built-ins, so a missing entry in
/// `tree.schemas` is expected and not an error.
const BUILTIN_TYPE_NAMES: &[&str] = &[
    "String", "Int", "Float", "Bool", "Null", "List", "Dict", "Bytes", "Date", "Time", "DateTime",
    "Duration", "Schema", "Any",
];

fn is_builtin(name: &str) -> bool {
    BUILTIN_TYPE_NAMES.contains(&name)
}

/// True when `name` corresponds to either a known schema declared
/// somewhere in the analyzed tree, or a built-in type the user may
/// extend with methods.
fn schema_known(name: &str, tree: &AnalyzedTree) -> bool {
    if is_builtin(name) {
        return true;
    }
    if tree
        .schemas
        .values()
        .any(|d| d.name.as_deref() == Some(name))
    {
        return true;
    }
    if tree.root_schemas.iter().any(|d| d.name == name) {
        return true;
    }
    false
}

/// Walk `root.directives`, ingest every `#extend Name with { ... }`
/// into `tree.schema_methods`. Must run after `collect_schemas` and
/// `collect_root_schemas` so the in-scope schema set is fully populated.
///
/// When the module has any `#import` directives, an unknown schema
/// name is *not* reported here — the workspace post-pass owns the
/// final visibility verdict, since the target may live behind one of
/// the imports. Single-file modules (no imports) take the strict
/// check directly so typos still surface in offline mode.
pub fn collect_extends(root: &Node, tree: &mut AnalyzedTree) {
    let has_imports = root
        .directives
        .iter()
        .any(|d| d.name == crate::directive_names::IMPORT);
    for dir in &root.directives {
        if dir.name != EXTEND {
            continue;
        }
        let DirectiveBody::NameBody {
            name,
            name_range,
            methods,
            ..
        } = &dir.body
        else {
            continue;
        };
        if !has_imports && !schema_known(name, tree) {
            tree.diagnostics.push(Diagnostic::ExtendUnknownSchema {
                name: name.clone(),
                range: span_of(*name_range),
            });
            continue;
        }
        let entry = tree.schema_methods.entry(name.clone()).or_default();
        // Detect conflicts against already-recorded methods (which
        // includes the schema's own `with { ... }` block, prior
        // `#extend` blocks earlier in the file, etc.). Source order
        // determines which is "first".
        let mut existing: HashMap<String, TokenRange> = HashMap::new();
        for m in entry.iter() {
            existing.entry(m.name.clone()).or_insert(m.name_range);
        }
        for parsed in methods {
            if let Some(prev_range) = existing.get(&parsed.name) {
                tree.diagnostics.push(Diagnostic::MethodNameConflict {
                    schema: name.clone(),
                    method: parsed.name.clone(),
                    first: span_of(*prev_range),
                    second: span_of(parsed.name_range),
                });
                continue;
            }
            existing.insert(parsed.name.clone(), parsed.name_range);
            entry.push(method_info_from_parser(parsed));
        }
    }
}

/// After both `collect_root_schemas` and `collect_extends` have run,
/// scan the per-schema method lists for *intra-block* duplicates that
/// the lowering passes did not catch (e.g. two methods of the same
/// name declared inside a single `with { ... }` block on a
/// `#schema X`). Emits one `MethodNameConflict` per duplicate pair.
pub fn check_method_uniqueness(tree: &mut AnalyzedTree) {
    let mut diags = Vec::new();
    for (schema_name, methods) in &tree.schema_methods {
        let mut seen: HashMap<&str, TokenRange> = HashMap::new();
        for m in methods {
            if let Some(prev) = seen.get(m.name.as_str()) {
                diags.push(Diagnostic::MethodNameConflict {
                    schema: schema_name.clone(),
                    method: m.name.clone(),
                    first: span_of(*prev),
                    second: span_of(m.name_range),
                });
            } else {
                seen.insert(m.name.as_str(), m.name_range);
            }
        }
    }
    tree.diagnostics.extend(diags);
}

/// Schema-rooted §J follow-up: warn when a method's own generic parameter
/// shadows one of its owning schema's generic parameters.
///
/// `#schema List<T> with { foo<T>(...) -> T: ... }` is accepted by the
/// parser and currently doesn't surface any diagnostic, but the
/// substitution path treats the two `T`s as the same binding key —
/// `resolve_call_signature` first substitutes the receiver's `T` (bound
/// from the runtime receiver), then `instantiate` rebinds the same `T`
/// against actual arg types. The end result is hard to read and
/// regression-prone. Renaming the method generic (`foo<U>(...)`) keeps
/// the binding keys distinct.
///
/// We only walk schemas the analyzer has SchemaDef metadata for —
/// built-in carriers like `core/list.relon` populate the schema by
/// name through the same path (`collect_schemas`), so `List<T>` /
/// `Dict<K, V>` participate without needing a special case.
///
/// Severity is Warning: the program still runs, the diagnostic just
/// flags the smell.
pub fn check_method_generic_shadowing(tree: &mut AnalyzedTree) {
    let mut diags = Vec::new();
    for def in tree.schemas.values() {
        if def.generics.is_empty() {
            continue;
        }
        let Some(schema_name) = def.name.as_deref() else {
            continue;
        };
        for m in &def.methods {
            if m.generics.is_empty() {
                continue;
            }
            for g in &m.generics {
                if def.generics.iter().any(|sg| sg == g) {
                    diags.push(Diagnostic::MethodGenericShadowsSchemaGeneric {
                        schema: schema_name.to_string(),
                        method: m.name.clone(),
                        generic: g.clone(),
                        range: span_of(m.name_range),
                    });
                }
            }
        }
    }
    tree.diagnostics.extend(diags);
}

/// Synthesize an [`FnSignature`] for `method`. `self` is *not* part of
/// the signature's parameter list — the receiver is identified by the
/// `(schema, method)` lookup key, so duplicating it as a leading param
/// would over-count arity at every call site. A method declared
/// `m(other: Self) -> Self` therefore becomes a 1-arg signature whose
/// `Self` types stay as `Self` placeholders; `resolve_call_signature`
/// substitutes the receiver's schema name when it has one to use.
fn synthesize_method_signature(schema: &str, method: &SchemaMethodInfo) -> FnSignature {
    let params = method
        .params
        .iter()
        .map(|p| FnParam {
            name: p.name.clone(),
            ty: p.type_node.clone(),
            optional: false,
        })
        .collect();
    FnSignature {
        name: format!("{schema}.{}", method.name),
        // Method-level generics (e.g. `map<U>` on `List<T>`) flow
        // through here so the existing `sig::instantiate` machinery
        // can bind them at the call site. Schema-level generics
        // (the `T` on `List<T>`) are *not* duplicated — they're
        // bound by the receiver type when `resolve_call_signature`
        // walks the path, and re-adding them here would shadow
        // those bindings with fresh placeholders.
        generics: method.generics.clone(),
        params,
        return_type: method.return_type.clone(),
        variadic_tail: None,
    }
}

/// Final step of the schema-rooted lowering: for every method recorded
/// in `tree.schema_methods`, populate `tree.method_signatures` with a
/// synthesized signature. Native methods and methods on schemas with
/// nameless declarations are skipped (the latter cannot be looked up
/// by `(schema, method)` anyway). Should run after the conflict
/// checks so that a duplicate-name pair only contributes a single
/// signature (the first); the second will already have produced a
/// `MethodNameConflict` diagnostic.
pub fn build_method_signature_table(tree: &mut AnalyzedTree) {
    let mut table: HashMap<(String, String), FnSignature> = HashMap::new();
    for (schema_name, methods) in &tree.schema_methods {
        for m in methods {
            let key = (schema_name.clone(), m.name.clone());
            if table.contains_key(&key) {
                continue;
            }
            table.insert(key, synthesize_method_signature(schema_name, m));
        }
    }
    tree.method_signatures = table;
}

/// Helper kept here so other passes don't have to know about
/// `type_node_simple` to construct receiver hints (`self` placeholder).
#[allow(dead_code)]
pub(crate) fn self_placeholder_type() -> relon_parser::TypeNode {
    type_node_simple("Self")
}
