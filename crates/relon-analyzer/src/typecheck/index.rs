//! Type-check sub-module: side-table builders consumed by the walker.
//!
//! Run once at the start of [`super::typecheck`] before the walker
//! visits any node:
//!
//! * `build_schema_index` — per-schema field-name → declared type.
//! * `build_enum_index` — per-enum schema → ordered variant names.
//! * `build_variant_field_index` — per-enum-variant → declared
//!   generic param names + field-name → type (seeded with the
//!   prelude `Result<T, E>` / `Option<T>` shapes).
//! * `build_base_index` — per-schema → declared base schemas (used by
//!   `subsumes_with` to accept a derived schema in a slot typed for
//!   one of its bases).
//! * `main_param_frame_for_typecheck` — synthesize a scope frame for
//!   the entry's `#main(...)` parameters so the walker sees them as
//!   bindings.
//! * `collect_pipe_target_calls` — pre-walk to mark FnCall NodeIds on
//!   the RHS of `|` so the FnCall arm can suppress the (intentionally
//!   one-short) arity check.
//!
//! Also home to the pure `substitute_generics_in_typenode` (re-exported
//! at the crate root for the evaluator) and the local type aliases
//! `SchemaIndex` / `EnumIndex` / `VariantFieldIndex` so the walker
//! struct and the domain sub-modules import a single source of truth.

use crate::infer::{self, SchemaBaseIndex};
use crate::resolve::ScopeFrame;
use crate::tree::AnalyzedTree;
use relon_parser::{child_nodes, Expr, Node, TypeNode};
use std::collections::{HashMap, HashSet};

/// Map from schema-name → field-name → declared type. Lets the
/// value-type check look up `User alice: { ... }` and validate each
/// inner field against `User`'s schema in one pass. Re-uses the
/// inference module's alias so both passes share the same shape.
pub(crate) type SchemaIndex = infer::SchemaIndex;

/// Map from sum-type Enum schema name → ordered set of its variant names.
/// Used by the exhaustiveness pass to compare match arms against the
/// declared variant list.
pub(super) type EnumIndex = HashMap<String, Vec<String>>;

/// v1.8 (C3 Result first-class): for every sum-type schema, record
/// per-variant field types together with the schema's declared
/// generic parameter names. This lets `check_generics` substitute
/// `Result<Int, String>` slot generics into `Ok { value: T }` /
/// `Err { error: E }` field types when the value is a variant
/// constructor literal.
///
/// Outer key: enum schema name. Inner key: variant name. Value:
/// `(generic_param_names, field_types)` where `generic_param_names`
/// is the schema's `<T, E>` declaration order (used to align with
/// the slot's `Result<Int, String>` generic args at the call site).
pub(super) type VariantFieldIndex =
    HashMap<String, HashMap<String, (Vec<String>, HashMap<String, TypeNode>)>>;

/// Mirror of `crate::resolve::main_param_frame` for the type-check
/// pass. Builds a synthetic frame populated with `#main(...)`
/// parameters so the walker sees them as bindings with their declared
/// types. Empty signatures and library files (no `#main`) yield `None`.
pub(super) fn main_param_frame_for_typecheck(
    tree: &AnalyzedTree,
    root_id: relon_parser::NodeId,
) -> Option<ScopeFrame> {
    let signature = tree.main_signature.as_ref()?;
    if signature.params.is_empty() {
        return None;
    }
    let mut frame = ScopeFrame::default();
    for param in &signature.params {
        frame.closure_params.insert(param.name.clone(), root_id);
        frame
            .closure_param_types
            .insert(param.name.clone(), param.type_node.clone());
    }
    Some(frame)
}

/// Pre-walk `root` and collect every FnCall NodeId that lives on the
/// RHS of a `|` pipe. The Stage 3.5 FnCall checker uses this set to
/// avoid flagging those calls — the pipe implicitly contributes the
/// LHS as their first positional argument, so the source-level arity
/// is intentionally one short.
pub(super) fn collect_pipe_target_calls(node: &Node, out: &mut HashSet<relon_parser::NodeId>) {
    if let Expr::Binary(relon_parser::Operator::Pipe, _left, right) = &*node.expr {
        if let Expr::FnCall { .. } = &*right.expr {
            out.insert(right.id);
        }
    }
    for child in child_nodes(node) {
        collect_pipe_target_calls(child, out);
    }
}

pub(crate) fn build_schema_index(tree: &AnalyzedTree) -> SchemaIndex {
    let mut index = SchemaIndex::new();
    for def in tree.schemas.values() {
        let Some(name) = &def.name else { continue };
        let mut fields = HashMap::new();
        for field in &def.fields {
            if let Some(t) = &field.type_hint {
                fields.insert(field.name.clone(), t.clone());
            }
        }
        index.insert(name.clone(), fields);
    }
    // v1.8e: pull in cross-module schema fields so the path-tail walker
    // can resolve `u.name` when `u` is typed as a `pkg.Schema`. Local
    // declarations win over imports — a module shadowing an imported
    // name is a project-side decision we don't second-guess.
    if let Some(idx) = tree.workspace_import_index.as_ref() {
        for (name, fields) in &idx.imported_schemas {
            index.entry(name.clone()).or_insert_with(|| fields.clone());
        }
    }
    index
}

pub(super) fn build_enum_index(tree: &AnalyzedTree) -> EnumIndex {
    let mut index = EnumIndex::new();
    for def in tree.schemas.values() {
        let Some(name) = &def.name else { continue };
        if def.variants.is_empty() {
            continue;
        }
        let variants: Vec<String> = def.variants.iter().map(|v| v.name.clone()).collect();
        index.insert(name.clone(), variants);
    }
    index
}

pub(super) fn build_variant_field_index(tree: &AnalyzedTree) -> VariantFieldIndex {
    let mut index = VariantFieldIndex::new();
    for def in tree.schemas.values() {
        let Some(name) = &def.name else { continue };
        if def.variants.is_empty() {
            continue;
        }
        let mut variants = HashMap::new();
        for variant in &def.variants {
            let mut fields = HashMap::new();
            for field in &variant.fields {
                if let Some(t) = &field.type_hint {
                    fields.insert(field.name.clone(), t.clone());
                }
            }
            variants.insert(variant.name.clone(), (def.generics.clone(), fields));
        }
        index.insert(name.clone(), variants);
    }
    seed_prelude_variants(&mut index);
    index
}

/// v1.8 C3: seed the analyzer's variant index with the same
/// `Result<T, E>` / `Option<T>` shapes the evaluator prelude
/// installs. Without this, a typed `Result<Int, String> r: ...`
/// binding would fall back to the silent-pass branch because the
/// analyzer's `tree.schemas` only records user-declared schemas.
fn seed_prelude_variants(index: &mut VariantFieldIndex) {
    use relon_parser::TokenRange;
    fn type_var(name: &str) -> TypeNode {
        TypeNode {
            path: vec![name.to_string()],
            generics: Vec::new(),
            is_optional: false,
            range: TokenRange::default(),
            variant_fields: None,
            doc_comment: None,
        }
    }
    // Result<T, E>
    let mut result_variants = HashMap::new();
    let mut ok_fields = HashMap::new();
    ok_fields.insert("value".to_string(), type_var("T"));
    result_variants.insert(
        "Ok".to_string(),
        (vec!["T".to_string(), "E".to_string()], ok_fields),
    );
    let mut err_fields = HashMap::new();
    err_fields.insert("error".to_string(), type_var("E"));
    result_variants.insert(
        "Err".to_string(),
        (vec!["T".to_string(), "E".to_string()], err_fields),
    );
    // Don't clobber a user-declared `Result` schema (allowed for
    // backward compat / shadow scenarios).
    index.entry("Result".to_string()).or_insert(result_variants);

    // Option<T>
    let mut option_variants = HashMap::new();
    let mut some_fields = HashMap::new();
    some_fields.insert("value".to_string(), type_var("T"));
    option_variants.insert("Some".to_string(), (vec!["T".to_string()], some_fields));
    option_variants.insert("None".to_string(), (vec!["T".to_string()], HashMap::new()));
    index.entry("Option".to_string()).or_insert(option_variants);
}

/// Substitute generic-parameter names with the corresponding
/// concrete TypeNodes from the slot's generic arguments. Used to
/// resolve `Ok { value: T }` against `Result<Int, String>`.
///
/// `pub` so the evaluator can reuse the same routine instead of
/// maintaining its own copy on `Value::Schema` field types — the two
/// were drifting independently (CHANGELOG v1.8b explicitly flagged this
/// as duplicate machinery). The function is pure (no `tree` access),
/// so exposing it doesn't widen the analyzer's contract.
pub fn substitute_generics_in_typenode(
    t: &TypeNode,
    subst: &HashMap<String, TypeNode>,
) -> TypeNode {
    if t.path.len() == 1 && t.generics.is_empty() {
        if let Some(replacement) = subst.get(&t.path[0]) {
            let mut clone = replacement.clone();
            clone.is_optional = clone.is_optional || t.is_optional;
            return clone;
        }
    }
    let mut out = t.clone();
    out.generics = out
        .generics
        .iter()
        .map(|g| substitute_generics_in_typenode(g, subst))
        .collect();
    out
}

/// Stage 2.3: walk every analyzed schema and record its direct base
/// schemas by name. Used by `InferredType::subsumes_with` to accept a
/// derived schema in a slot expecting one of its bases.
pub(crate) fn build_base_index(tree: &AnalyzedTree) -> SchemaBaseIndex {
    let mut index = SchemaBaseIndex::new();
    for def in tree.schemas.values() {
        let Some(name) = &def.name else { continue };
        let bases: Vec<String> = def.bases.iter().map(|b| b.name.clone()).collect();
        if !bases.is_empty() {
            index.insert(name.clone(), bases);
        }
    }
    index
}
