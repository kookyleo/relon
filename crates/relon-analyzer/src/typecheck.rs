//! Static type-check pass.
//!
//! Statically classifiable type errors are surfaced as `Error`-severity
//! diagnostics — the analyzer is the source of truth for anything
//! derivable from source + schemas alone, and the facade refuses to
//! enter the evaluator while errors are present (see
//! `Diagnostic::severity`). The evaluator's runtime `check_type` only
//! handles the residual cases the static pass can't see (host-pushed
//! `#main(...)` args, dynamic `#brand`, `Match` arms over a runtime
//! value, etc.).
//!
//! Two complementary findings live here:
//!
//! * [`UnresolvedReference`] — a `&sibling.X` / `Variable(X)` whose
//!   head couldn't be statically bound *and* no spread / closure
//!   binding on the active scope chain could plausibly save it.
//!   Surfaced as `Warning` because the analyzer's view is conservative.
//! * [`StaticTypeMismatch`] — a typed schema binding whose value
//!   expression has a determinable shape (literal, list, dict, type)
//!   that disagrees with the field's declared type. Surfaced as
//!   `Error`.
//!
//! [`UnresolvedReference`]: crate::Diagnostic::UnresolvedReference
//! [`StaticTypeMismatch`]: crate::Diagnostic::StaticTypeMismatch
//!
//! ## Walker pass map
//!
//! The `Walker` impl below carries 30 methods that all share three
//! pieces of mutable state: `tree.diagnostics`, `scope_stack`, and
//! `schema_index` (plus a few smaller side-tables). The methods are
//! grouped by responsibility below — there are no separate trait
//! passes because the coupling on that shared state makes splitting
//! into independent visitor passes negative-ROI (a prior P3 audit
//! over-counted by folding `mod tests` free fns into the impl total
//! and recommended a refactor that turned out to be unwarranted; this
//! map is the minimal landing of that audit's actual finding —
//! discoverability, not decomposition). The size is on par with
//! rustc's / clippy's analogous walkers.
//!
//! - **dispatch** — `visit`, `visit_internal` (the ~290-line
//!   `match &*node.expr` that fans out to every other group).
//! - **fn_call** — `check_unresolved_fn_call`, `check_fn_call`,
//!   `resolve_call_signature`, `check_strict_fn_call`.
//! - **const_fold + binary** — `check_binary_mismatch`,
//!   `check_const_fold`.
//! - **dict v1.3 / spread bypass** — `check_dict_v1_3`,
//!   `schema_known`, `spread_source_schema`, `spread_source_is_dict`,
//!   `spread_contributed_keys`.
//! - **scope construction** — `build_type_scope`,
//!   `build_type_scope_with_closure`.
//! - **match / closure return** — `check_match_arm_types`,
//!   `check_closure_return`, `check_match_exhaustiveness`,
//!   `infer_enum_type`.
//! - **reference / variable / path tail** — `check_unresolved_ref`,
//!   `check_unresolved_var`, `check_strict_path`, `check_path_tail`.
//! - **small helpers** — `is_known_fn`, `lookup_field_node`,
//!   `dynamic_save`.
//! - **typed binding + custom schema + generics** —
//!   `check_typed_binding`, `check_generics`,
//!   `check_against_custom_schema`, `build_generic_subst`.

use crate::diagnostic::{span_of, Diagnostic};
use crate::generics::collect_bindings;
use crate::infer::{self, infer_type, InferredType, SchemaBaseIndex, TypeScope};
use crate::resolve::{build_frame, path_head, ScopeFrame};
use crate::sig::{instantiate, lookup_signature, type_node_simple, FnParam, FnSignature};
use crate::tree::AnalyzedTree;
use relon_parser::{
    child_nodes, ClosureParam, Expr, Node, RefBase, TokenKey, TokenRange, TypeNode,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Run the type-check walker over `root` and append diagnostics to
/// `tree`. Must be called after [`crate::resolve::resolve_references`]
/// and [`crate::schema::collect_schemas`] so the side-tables they
/// produce are available.
pub fn typecheck(root: &Node, tree: &mut AnalyzedTree) {
    // Collect static type info and field bindings for each declared
    // schema so the value-type pass can look fields up by name.
    let schema_index = build_schema_index(tree);
    let enum_index = build_enum_index(tree);
    let variant_field_index = build_variant_field_index(tree);
    let base_index = build_base_index(tree);

    let mut pipe_target_calls = HashSet::new();
    collect_pipe_target_calls(root, &mut pipe_target_calls);

    // v1.3: seed the entry's `#main(...)` parameters into a synthetic
    // root frame so the type-check walker sees them as bindings with
    // their declared types — same logic the resolver uses (mirrors
    // `crate::resolve::main_param_frame`). Without this, root bodies
    // like `#main(Int n) -> String\nn+1` would have `n` typed as `Any`
    // and the `MainReturnTypeMismatch` check would silently skip.
    let mut scope_stack: Vec<ScopeFrame> = Vec::new();
    if let Some(frame) = main_param_frame_for_typecheck(tree, root.id) {
        scope_stack.push(frame);
    }

    let mut walker = Walker {
        tree,
        scope_stack,
        schema_index,
        enum_index,
        variant_field_index,
        base_index,
        pipe_target_calls,
    };
    walker.visit(root);
}

/// Mirror of `crate::resolve::main_param_frame` for the type-check
/// pass. Builds a synthetic frame populated with `#main(...)`
/// parameters so the walker sees them as bindings with their declared
/// types. Empty signatures and library files (no `#main`) yield `None`.
fn main_param_frame_for_typecheck(
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
fn collect_pipe_target_calls(node: &Node, out: &mut HashSet<relon_parser::NodeId>) {
    if let Expr::Binary(relon_parser::Operator::Pipe, _left, right) = &*node.expr {
        if let Expr::FnCall { .. } = &*right.expr {
            out.insert(right.id);
        }
    }
    for child in child_nodes(node) {
        collect_pipe_target_calls(child, out);
    }
}

/// Map from schema-name → field-name → declared type. Lets the
/// value-type check look up `User alice: { ... }` and validate each
/// inner field against `User`'s schema in one pass. Re-uses the
/// inference module's alias so both passes share the same shape.
pub(crate) type SchemaIndex = infer::SchemaIndex;

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

/// Map from sum-type Enum schema name → ordered set of its variant names.
/// Used by the exhaustiveness pass to compare match arms against the
/// declared variant list.
type EnumIndex = HashMap<String, Vec<String>>;

fn build_enum_index(tree: &AnalyzedTree) -> EnumIndex {
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
type VariantFieldIndex = HashMap<String, HashMap<String, (Vec<String>, HashMap<String, TypeNode>)>>;

fn build_variant_field_index(tree: &AnalyzedTree) -> VariantFieldIndex {
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

struct Walker<'a> {
    tree: &'a mut AnalyzedTree,
    scope_stack: Vec<ScopeFrame>,
    schema_index: SchemaIndex,
    enum_index: EnumIndex,
    /// v1.8 C3: per-variant field types for sum-type schemas, used by
    /// `check_generics` to recurse into a variant constructor literal
    /// (`Result.Ok { value: 42 }`) against a typed slot
    /// (`Result<Int, String>`) with proper generic substitution.
    variant_field_index: VariantFieldIndex,
    base_index: SchemaBaseIndex,
    /// Stage 3.5: NodeIds of FnCall expressions that appear on the RHS
    /// of a `|` pipe operator. The pipe implicitly supplies the LHS as
    /// the call's first positional argument, so the static arity /
    /// type check would false-flag (`range(5) | len()` would otherwise
    /// flag `len()` as missing its 1 arg). The walker pre-collects
    /// these ids before recursing so the FnCall arm can suppress the
    /// signature check on them.
    pipe_target_calls: HashSet<relon_parser::NodeId>,
}

impl<'a> Walker<'a> {
    fn visit(&mut self, node: &Node) {
        self.visit_internal(node, None);
    }

    fn visit_internal(&mut self, node: &Node, field_name: Option<&str>) {
        // Type binding check first — `Type field: value` carries info
        // that lets us validate `value` immediately, before recursing.
        if let Some(t) = &node.type_hint {
            self.check_typed_binding(t, node, field_name.unwrap_or("_"));

            // If the type-hint is a custom schema, also walk the value's
            // dict fields against the schema's expected types.
            self.check_against_custom_schema(t, node);

            // v1.6: ban `Any` anywhere in the typed-binding annotation.
            // Catches both bare `Any field: ...` and nested
            // `List<Any> xs: ...` forms.
            let mut to_emit: Vec<Diagnostic> = Vec::new();
            crate::ban_unsafe_types::scan_typenode_for_any(
                t,
                &format!("typed binding `{}`", field_name.unwrap_or("_")),
                &mut to_emit,
            );
            self.tree.diagnostics.extend(to_emit);
        }

        match &*node.expr {
            Expr::Dict(pairs) => {
                // v1.4: pre-register every sibling closure's signature
                // *before* `check_dict_v1_3` runs so typed-spread
                // resolution (`...sibling_call()`) can look up the
                // declared return type. Without this pre-pass the
                // signature wouldn't be visible until the closure-arm
                // of `visit_internal` fires later in the iteration.
                for (key, value) in pairs {
                    if let (
                        TokenKey::String(name, _, _),
                        Expr::Closure {
                            params,
                            return_type,
                            body,
                        },
                    ) = (key, &*value.expr)
                    {
                        self.tree
                            .field_closure_index
                            .insert(name.to_string(), value.id);
                        let sig = extract_closure_signature(value, params, return_type, body);
                        self.tree.closure_signatures.insert(value.id, sig);
                    }
                }
                // v1.3: validate spread / dynamic-key typehints under
                // strict mode, and surface DuplicateField clashes at
                // every mode (strict-or-not). The check runs *before*
                // we recurse so a typed spread that contributes a
                // schema's keys to the frame's static set still affects
                // the visible field set seen by the children.
                self.check_dict_v1_3(pairs);
                let frame = build_frame(pairs);
                self.scope_stack.push(frame);
                for (key, value) in pairs {
                    let field_name = if let TokenKey::String(name, _, _) = key {
                        Some(name.as_str())
                    } else {
                        None
                    };
                    // Stage 3.3: when a dict field's value is itself a
                    // closure, index the field name → closure NodeId so
                    // sibling-callable lookups (`{ f(x): x, y: f(1) }`)
                    // can find the static signature without re-walking.
                    // Idempotent re-insert here keeps the original
                    // Stage 3.3 invariant for shapes that don't go
                    // through the v1.4 pre-pass (no String key).
                    if let (Some(name), Expr::Closure { .. }) = (field_name, &*value.expr) {
                        self.tree
                            .field_closure_index
                            .insert(name.to_string(), value.id);
                    }
                    // v1.5: under strict mode every untyped dict
                    // value must produce a derivable type. Skip
                    // typed slots (the typed-binding walker already
                    // owns those) and references / variables (their
                    // own arms route through `check_strict_path`).
                    if self.tree.strict_mode
                        && value.type_hint.is_none()
                        && matches!(key, TokenKey::String(_, _, _))
                        && !matches!(
                            &*value.expr,
                            Expr::Variable(_)
                                | Expr::Reference { .. }
                                | Expr::Closure { .. }
                                | Expr::Dict(_)
                                | Expr::List(_)
                        )
                    {
                        let any_result = {
                            let scope = self.build_type_scope();
                            let t = infer_type(value, &scope).unwrap_or(InferredType::Any);
                            matches!(t, InferredType::Any)
                        };
                        if any_result {
                            self.tree
                                .diagnostics
                                .push(Diagnostic::ExpressionTypeUnknown {
                                    reason: format!(
                                        "dict field `{}` value type is not statically derivable",
                                        field_name.unwrap_or("_")
                                    ),
                                    range: span_of(value.range),
                                });
                        }
                    }
                    self.visit_internal(value, field_name);
                }
                self.scope_stack.pop();
            }
            Expr::Closure {
                params,
                body,
                return_type,
            } => {
                let mut frame = ScopeFrame::default();
                for param in params {
                    frame.closure_params.insert(param.name.clone(), body.id);
                    if let Some(t) = &param.type_hint {
                        frame
                            .closure_param_types
                            .insert(param.name.clone(), t.clone());
                    }
                }
                // v1.5: under strict mode every closure parameter must
                // declare a type — an untyped param defaults to `Any`,
                // which leaks an unclassified type into the body's
                // inference scope. Pin the diagnostic on the closure's
                // own range (the param spans aren't on `ClosureParam`).
                if self.tree.strict_mode {
                    for param in params {
                        if param.type_hint.is_none() {
                            self.tree
                                .diagnostics
                                .push(Diagnostic::ClosureParamTypeMissing {
                                    param_name: param.name.clone(),
                                    range: span_of(node.range),
                                });
                        }
                    }
                }
                // v1.6: ban `Any` (recursively) in closure param types
                // and the optional `-> ReturnType` annotation. Fires
                // in *every* mode; strict and non-strict alike are
                // expected to drop `Any` from user code.
                {
                    let mut to_emit: Vec<Diagnostic> = Vec::new();
                    for param in params {
                        if let Some(t) = &param.type_hint {
                            crate::ban_unsafe_types::scan_typenode_for_any(
                                t,
                                &format!("closure parameter `{}`", param.name),
                                &mut to_emit,
                            );
                        }
                    }
                    if let Some(rt) = return_type {
                        crate::ban_unsafe_types::scan_typenode_for_any(
                            rt,
                            "closure return type",
                            &mut to_emit,
                        );
                    }
                    self.tree.diagnostics.extend(to_emit);
                }
                // Stage 3.3: extract the closure's signature *before*
                // walking the body so any nested FnCall to a sibling-
                // bound recursive closure resolves against the same
                // signature. The walker doesn't carry the closure's own
                // NodeId (it's the parent node's id), so we record it
                // against the field-value node id supplied by the dict
                // arm via `field_closure_index`.
                let sig = extract_closure_signature(node, params, return_type, body);
                self.tree.closure_signatures.insert(node.id, sig);
                self.scope_stack.push(frame);
                if let Some(declared_return) = return_type {
                    self.check_closure_return(params, body, declared_return, field_name);
                } else if self.tree.strict_mode {
                    // No declared `-> ReturnType` — the closure's
                    // signature falls back to the body's inferred
                    // type. Strict mode demands that body inference
                    // succeed; if it lands on `Any` we can't
                    // distinguish a typed-`Any` slot from a real
                    // failure, so push a closure-body diagnostic.
                    let scope = self.build_type_scope_with_closure(params);
                    let body_ty = infer_type(body, &scope).unwrap_or(InferredType::Any);
                    if matches!(body_ty, InferredType::Any) {
                        self.tree
                            .diagnostics
                            .push(Diagnostic::ClosureReturnTypeUnknown {
                                role: field_name.unwrap_or("<closure>").to_string(),
                                range: span_of(body.range),
                            });
                    }
                }
                self.visit_internal(body, None);
                self.scope_stack.pop();
            }
            Expr::List(items) => {
                // v1.5: under strict mode, every list element must
                // produce a derivable type. We collect diagnostics
                // into a local vec so the inference scope's read-only
                // borrow of `self.tree` stays disjoint from the push.
                if self.tree.strict_mode {
                    let mut to_emit: Vec<Diagnostic> = Vec::new();
                    {
                        let scope = self.build_type_scope();
                        for item in items {
                            // Skip type-hinted items — the typed-
                            // binding pass already covers their slot.
                            if item.type_hint.is_some() {
                                continue;
                            }
                            let t = infer_type(item, &scope).unwrap_or(InferredType::Any);
                            if matches!(t, InferredType::Any)
                                && !matches!(
                                    &*item.expr,
                                    Expr::Variable(_) | Expr::Reference { .. }
                                )
                            {
                                to_emit.push(Diagnostic::ExpressionTypeUnknown {
                                    reason: "list element type is not statically derivable"
                                        .to_string(),
                                    range: span_of(item.range),
                                });
                            }
                        }
                    }
                    self.tree.diagnostics.extend(to_emit);
                }
                for item in items {
                    self.visit_internal(item, None);
                }
            }
            Expr::Reference { base, path } => {
                self.check_unresolved_ref(node, base, path);
            }
            Expr::Variable(path) => {
                self.check_unresolved_var(node, path);
                // Schema-rooted §J follow-up: a Dynamic segment in a
                // path lowers to `receiver.index(key)`. When the
                // receiver schema declares a concrete `index(key:
                // K)` parameter type, validate the dynamic key
                // expression's static type matches; otherwise the
                // mismatch would only surface at runtime from inside
                // the witness body.
                self.check_index_dispatch(path);
            }
            Expr::Match { expr, arms } => {
                self.check_match_exhaustiveness(node.range, expr, arms);
                self.check_match_arm_types(node.range, expr, arms);
                self.visit_internal(expr, None);
                for (pat, body) in arms {
                    self.visit_internal(pat, None);
                    self.visit_internal(body, None);
                }
            }
            Expr::Binary(op, left, right) => {
                self.check_binary_mismatch(node, *op, left, right, field_name);
                // Stage 5: literal-only arithmetic — divide-by-zero
                // and i64 overflow get surfaced statically. When the
                // outer node folds to an error we *don't* recurse,
                // because the same diagnostic would re-fire on every
                // failing intermediate sub-node (`(1+2)*(3+4)/0` →
                // both `0/0`'s parent and the outer node would
                // double-report). Successful or non-foldable nodes
                // still walk their children so we don't drop any
                // sibling-arithmetic errors hidden under a non-
                // arithmetic head.
                if !self.check_const_fold(node) {
                    self.visit_internal(left, None);
                    self.visit_internal(right, None);
                }
            }
            Expr::Unary(_, inner) => {
                // Stage 5: same treatment as Binary — `-i64::MIN`
                // overflows at fold time and we shouldn't continue
                // down into the literal child once we've reported.
                if !self.check_const_fold(node) {
                    self.visit_internal(inner, None);
                }
            }
            Expr::FnCall { path, args } => {
                self.check_unresolved_fn_call(node, path);
                self.check_fn_call(node, path, args);
                self.check_strict_fn_call(node, path);
                self.check_method_dispatch(node, path);
                for arg in args {
                    self.visit_internal(&arg.value, None);
                }
            }
            _ => {
                for child in child_nodes(node) {
                    self.visit_internal(child, None);
                }
            }
        }
    }

    /// Stage 2.7: when a function call's `callable` is a single-segment
    /// bare name and the analyzer can prove the name isn't bound — not
    /// a closure param, not a sibling, not in `host_fn_names ∪
    /// stdlib_names()` — surface `UnresolvedReference`. Multi-segment
    /// callables (`obj.method(...)`) are deferred to a later stage.
    fn check_unresolved_fn_call(&mut self, node: &Node, path: &[TokenKey]) {
        if path.len() != 1 {
            return;
        }
        let TokenKey::String(name, _, _) = &path[0] else {
            return;
        };
        if self.dynamic_save(name) || self.is_known_fn(name) {
            return;
        }
        // Sibling-bound name? `{ helper(): 1, x: helper() }` — the
        // resolver pass binds `helper` as a Variable head, but `FnCall`
        // doesn't go through `Reference`/`Variable`, so we have to walk
        // the scope chain ourselves.
        for frame in self.scope_stack.iter().rev() {
            if frame.fields.contains_key(name) || frame.closure_params.contains_key(name) {
                return;
            }
        }
        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
            name: name.clone(),
            range: span_of(node.range),
        });
    }

    /// Stage 3.5 / 3.6: validate a `FnCall` against its static
    /// signature when one is reachable. Three sources, in order:
    ///
    /// 1. A multi-segment path whose head resolves to a sibling dict
    ///    field whose value is a closure (Stage 3.6 dict-literal
    ///    sibling form).
    /// 2. A single-segment name resolved through
    ///    [`lookup_signature`] (closure index → host fns → stdlib).
    ///
    /// Anything not in those tables silently passes — runtime keeps
    /// owning the verdict for cross-module fns, dynamic refs, etc.
    fn check_fn_call(&mut self, node: &Node, path: &[TokenKey], args: &[relon_parser::CallArg]) {
        // Pipe RHS: the LHS supplies the implicit first arg, so the
        // source-level arity is intentionally one short. Skipping
        // here keeps the analyzer in lock-step with the runtime's
        // pipe semantics (`call_function` prepends the LHS value).
        if self.pipe_target_calls.contains(&node.id) {
            return;
        }
        let Some(sig) = self.resolve_call_signature(path) else {
            return;
        };
        let display_name = sig.name.clone();
        let positional_count = args.iter().filter(|a| a.name.is_none()).count();
        // v1 only validates positional args; named args silently pass
        // (they would just shadow positions or be redundant). Bail out
        // early when any named arg is present so we don't false-flag.
        if positional_count != args.len() {
            return;
        }
        // Arity check honoring optional tail params and variadic_tail.
        let (required, max_fixed) = required_and_max(&sig);
        let in_range = if sig.variadic_tail.is_some() {
            args.len() >= required
        } else {
            args.len() >= required && args.len() <= max_fixed
        };
        if !in_range {
            let expected = if sig.variadic_tail.is_some() {
                format!("at least {required}")
            } else if required == max_fixed {
                format!("{required}")
            } else {
                format!("{required} to {max_fixed}")
            };
            self.tree
                .diagnostics
                .push(Diagnostic::FnCallArgCountMismatch {
                    fn_name: display_name,
                    expected,
                    found: args.len(),
                    range: span_of(node.range),
                });
            return;
        }

        // Per-arg type check. Arguments past the fixed list are
        // validated against `variadic_tail`. Arguments mapped to
        // optional params still check, but a missing optional is fine
        // (already accepted above by `required <= len`).
        //
        // We collect diagnostics into a local Vec so the inference
        // scope's read-only borrow of `self.tree` doesn't collide with
        // the diagnostic push's mutable borrow.
        //
        // v1.1: when `sig.generics` is non-empty, run unification over
        // every (param_ty, arg_ty) pair first to collect placeholder
        // bindings, then instantiate the signature so each per-arg
        // subsumption check sees a concrete type. The substitution
        // is shared with the FnCall return-type inference path in
        // `infer::infer_type` so the rest of the analyzer reads the
        // tightened type back out as `List<Int>` etc.
        let mut to_emit: Vec<Diagnostic> = Vec::new();
        {
            let scope = self.build_type_scope();
            let working_sig = if sig.generics.is_empty() {
                sig.clone()
            } else {
                let bindings = collect_bindings(&sig, args, &scope);
                instantiate(&sig, &bindings)
            };
            for (idx, arg) in args.iter().enumerate() {
                let (param_name, expected_ty) = if idx < working_sig.params.len() {
                    (
                        working_sig.params[idx].name.clone(),
                        working_sig.params[idx].ty.clone(),
                    )
                } else if let Some(tail_ty) = &working_sig.variadic_tail {
                    (
                        format!("rest[{}]", idx - working_sig.params.len()),
                        tail_ty.clone(),
                    )
                } else {
                    continue;
                };
                let Some(arg_ty) = infer_type(&arg.value, &scope) else {
                    continue;
                };
                if arg_ty.subsumes_with_imports(
                    &expected_ty,
                    Some(&self.base_index),
                    self.tree.workspace_import_index.as_ref(),
                ) {
                    continue;
                }
                to_emit.push(Diagnostic::FnCallArgTypeMismatch {
                    fn_name: display_name.clone(),
                    param_name,
                    expected: format_type(&expected_ty),
                    found: arg_ty.name(),
                    range: span_of(arg.value.range),
                });
            }
        }
        self.tree.diagnostics.extend(to_emit);
    }

    /// Stage 3.5: resolve a call path to its static signature.
    /// Single-segment paths go through the global lookup
    /// (closure-index → host fns → stdlib). Multi-segment paths fall
    /// back to the Stage 3.6 dict-literal sibling form: the head must
    /// name a sibling whose value is a Dict literal, and the second
    /// segment must name a key in that dict whose value is a closure.
    /// Anything else returns `None` (silent fall-through).
    fn resolve_call_signature(&self, path: &[TokenKey]) -> Option<FnSignature> {
        if path.is_empty() {
            return None;
        }
        // Head must be a String key — Spread / synthetic keys can never
        // produce a callable.
        let TokenKey::String(head, _, _) = &path[0] else {
            return None;
        };
        if path.len() == 1 {
            return lookup_signature(head, self.tree, &self.tree.host_fn_signatures);
        }
        // 2-segment paths: dict-literal sibling closure, cross-module
        // aliased closure, and the head-as-schema dispatch all live
        // here. 3+ segment paths fall through to schema-rooted
        // multi-hop dispatch at the bottom (no sibling-dict /
        // aliased-import shape applies there — those tables are keyed
        // by head name alone).
        let last_idx = path.len() - 1;
        let TokenKey::String(method, _, _) = &path[last_idx] else {
            return None;
        };
        if path.len() == 2 {
            // Try sibling-field dict-literal first (Stage 3.6). Falls
            // through to the v1.1 cross-module index when there's no
            // sibling field with that name.
            if let Some(target) = self.lookup_field_node(head) {
                // Only walk literal dicts — abstract types (FnCall /
                // Reference / typed schema bindings) silently fall
                // through to runtime.
                if target.type_hint.is_none() {
                    if let Expr::Dict(inner_pairs) = &*target.expr {
                        for (k, v) in inner_pairs {
                            if let TokenKey::String(name, _, _) = k {
                                if name == method {
                                    if matches!(&*v.expr, Expr::Closure { .. }) {
                                        let mut sig =
                                            self.tree.closure_signatures.get(&v.id).cloned()?;
                                        sig.name = format!("{head}.{method}");
                                        return Some(sig);
                                    }
                                    return None;
                                }
                            }
                        }
                    }
                }
            }
            // v1.1: cross-module alias.method — `#import alias from
            // "lib"` exposes the imported module's top-level closures
            // under `alias.method`.
            if let Some(idx) = self.tree.workspace_import_index.as_ref() {
                if let Some(methods) = idx.aliased_closures.get(head) {
                    if let Some(sig) = methods.get(method) {
                        return Some(sig.clone());
                    }
                }
            }
        }
        // Schema-rooted Phase B: schema method dispatch. Three flavors:
        //
        //   1. `value.method(args)` — `value` is a 1-segment binding
        //      whose static type is some `Schema(name)`. Look up the
        //      method on that schema's table.
        //   2. `Schema.method(args)` — `Schema` itself is the head,
        //      static (no receiver) dispatch; lookup keyed by the
        //      schema name regardless.
        //   3. `head.f1.f2…fk.method(args)` (multi-hop) — walk
        //      `path[..-1]` through the inference engine; when it
        //      terminates on `Schema(name)` we dispatch the last
        //      segment against that schema's method table. Mirrors
        //      the v1.4 path-tail machinery `infer::walk_path`
        //      already powers for spreads / strict mode.
        //
        // For case 1, the receiver type comes from the path-walker so
        // schema-typed sibling fields, schema-typed `#main(p: T)`
        // params, and `let`-style closure params all participate.
        if let Some(receiver_schema) = self.resolve_method_receiver_prefix(&path[..last_idx]) {
            if let Some(sig) = self
                .tree
                .method_signatures
                .get(&(receiver_schema, method.clone()))
            {
                return Some(sig.clone());
            }
        }
        None
    }

    /// Schema-rooted Phase B: emit `UnknownMethod` when a 2-segment
    /// `head.method(...)` call has a head whose static type resolves
    /// to a known schema, but the method isn't recorded on that
    /// schema's table. Also enforces `#private` visibility — a private
    /// method may only be called from another method on the *same*
    /// schema (currently approximated as: from inside the same
    /// `with { ... }` block, tracked via `self.method_call_context`).
    ///
    /// Skipped when the head doesn't resolve to a schema receiver: the
    /// existing single-segment `UnresolvedReference` /
    /// `UnknownTypeName` machinery already covers that, and we must
    /// not double-count names like sibling closures or aliased imports.
    fn check_method_dispatch(&mut self, node: &Node, path: &[TokenKey]) {
        if path.len() < 2 {
            return;
        }
        let TokenKey::String(head, _, _) = &path[0] else {
            return;
        };
        let last_idx = path.len() - 1;
        let TokenKey::String(method, method_range, _) = &path[last_idx] else {
            return;
        };
        // For the 2-segment form only, head-as-sibling-closure or
        // head-as-aliased-import is a non-schema dispatch — let the
        // regular signature checks handle it. With 3+ segments, the
        // head is necessarily the root of a path walk (sibling
        // closures and aliased imports don't have nested fields the
        // walker would descend through), so we keep the multi-hop
        // path-walk authoritative.
        if path.len() == 2 {
            if self.lookup_field_node(head).is_some() {
                return;
            }
            if let Some(idx) = self.tree.workspace_import_index.as_ref() {
                if idx.aliased_closures.contains_key(head) {
                    return;
                }
            }
        }
        let Some(schema) = self.resolve_method_receiver_prefix(&path[..last_idx]) else {
            return;
        };
        let key = (schema.clone(), method.clone());
        let Some(info) = self
            .tree
            .schema_methods
            .get(&schema)
            .and_then(|methods| methods.iter().find(|m| &m.name == method))
            .cloned()
        else {
            self.tree.diagnostics.push(Diagnostic::UnknownMethod {
                schema,
                method: method.clone(),
                range: span_of(*method_range),
            });
            return;
        };
        if info.is_private && !self.in_method_block(&schema) {
            self.tree
                .diagnostics
                .push(Diagnostic::PrivateMethodViolation {
                    schema,
                    method: method.clone(),
                    range: span_of(*method_range),
                });
        }
        // Suppress unused-key warning until we wire in argument
        // type-checking against the synthesized method signature: the
        // existing `check_fn_call` already validated arity once
        // `resolve_call_signature` returned the method's `FnSignature`.
        let _ = (key, node);
    }

    /// Schema-rooted §J follow-up: walk `path` for any `Dynamic`
    /// segments. Each one is the bracket-form `a[expr]` desugar
    /// landing site — at runtime it dispatches through the receiver
    /// schema's `index(key: ...)` witness. When that witness's
    /// declared key parameter has a concrete type (after constraint-
    /// generic substitution, e.g. `index(key: Int)`), validate the
    /// dynamic key's inferred type against it. Mismatch surfaces as
    /// `MethodGenericArgMismatch`; without this check the same
    /// disagreement only fires at runtime from inside the witness
    /// body.
    ///
    /// The receiver type is recovered by walking `path[..i]` through
    /// `walk_path`. If the prefix doesn't land on a schema we have a
    /// method table for (or the schema has no `index` method, or its
    /// `index` declares `key: K` with `K` still polymorphic), we
    /// silently skip — runtime still owns those cases.
    fn check_index_dispatch(&mut self, path: &[TokenKey]) {
        // Walk the segments looking for a Dynamic — each one is an
        // independent index call against the prefix up to (but not
        // including) the segment itself.
        let scope = self.build_type_scope();
        let mut to_emit: Vec<Diagnostic> = Vec::new();
        for (idx, seg) in path.iter().enumerate() {
            let TokenKey::Dynamic(key_node, _is_optional) = seg else {
                continue;
            };
            if idx == 0 {
                // A leading Dynamic would mean `[expr]` at the root,
                // which the parser never produces — but be defensive.
                continue;
            }
            let prefix = &path[..idx];
            let schema_name = match infer::walk_path(prefix, &scope) {
                infer::PathTailOutcome::Resolved(InferredType::Schema(name)) => name,
                _ => continue,
            };
            // Locate the synthesized signature so we have both the
            // method's generics list and the param's TypeNode. The
            // method-signature table is the single source of truth
            // after `build_method_signature_table`.
            let key = (schema_name.clone(), "index".to_string());
            let Some(sig) = self.tree.method_signatures.get(&key).cloned() else {
                continue;
            };
            let Some(key_param) = sig.params.first() else {
                continue;
            };
            // Skip when the param type is still a polymorphic
            // placeholder. The receiver-schema's generics shadow the
            // method's own — both must be subtracted before declaring
            // a leftover name "polymorphic". Concrete types
            // (`Int`/`String`/builtins/user schemas) pass.
            if param_is_polymorphic(&key_param.ty, &sig.generics, &schema_name, self.tree) {
                continue;
            }
            let Some(arg_ty) = infer_type(key_node, &scope) else {
                continue;
            };
            // `Any` here is the closure-param-without-type leak (the
            // only Any source left after v1.6); don't double-flag.
            if matches!(arg_ty, InferredType::Any) {
                continue;
            }
            if arg_ty.subsumes_with_imports(
                &key_param.ty,
                Some(&self.base_index),
                self.tree.workspace_import_index.as_ref(),
            ) {
                continue;
            }
            to_emit.push(Diagnostic::MethodGenericArgMismatch {
                schema: schema_name,
                method: "index".to_string(),
                param_name: key_param.name.clone(),
                expected: format_type(&key_param.ty),
                found: arg_ty.name(),
                range: span_of(key_node.range),
            });
        }
        self.tree.diagnostics.extend(to_emit);
    }

    /// Stub for the in-method-block tracking hook used by
    /// `check_method_dispatch`. The full implementation needs the
    /// type-check walker to push / pop a per-method context as it
    /// enters each `with { ... }` block; today we have no such
    /// machinery, so the conservative answer is "no" — which means a
    /// `#private` method called from anywhere outside its declaration
    /// site (including from sibling methods on the same schema) is
    /// flagged. This is a known false-positive surface that the
    /// follow-up sub-task wires up properly.
    fn in_method_block(&self, _schema: &str) -> bool {
        false
    }

    /// Resolve the receiver schema name of a method call's head segment.
    /// Returns the schema name if either:
    ///   * `head` is itself a schema name in scope (the static
    ///     `Schema.method` form), or
    ///   * `head` is a binding whose static type chain ends at a
    ///     `Schema(name)`.
    fn resolve_method_receiver(&self, head: &str) -> Option<String> {
        // Static `Schema.method(...)` form: head is a known schema
        // name. Any name registered in `schema_index` (whether via a
        // `#schema` directive, an enum, or a base reference) qualifies.
        if self.schema_index.contains_key(head) {
            return Some(head.to_string());
        }
        // Even if the schema has no `+ Base` chain (and hence no entry
        // in `schema_index`), a name explicitly recorded in
        // `schema_methods` is still a valid receiver root — that
        // covers schemas declared via `#schema X with { ... }` whose
        // body is empty / `Enum<...>` and never propagated into the
        // base index.
        if self.tree.schema_methods.contains_key(head) {
            return Some(head.to_string());
        }
        let scope = self.build_type_scope();
        let single_path = [TokenKey::String(
            head.to_string(),
            TokenRange::default(),
            false,
        )];
        if let infer::PathTailOutcome::Resolved(InferredType::Schema(name)) =
            infer::walk_path(&single_path, &scope)
        {
            return Some(name);
        }
        None
    }

    /// Multi-hop receiver resolution: walk `path[..-1]` and return the
    /// schema name when the walk lands on `Schema(name)`. The last
    /// segment is the method name and is *not* included in the walk.
    ///
    /// For `path.len() == 2` this reduces to single-name head lookup
    /// (mirrors [`Self::resolve_method_receiver`]).
    /// For `path.len() >= 3` (`o.customer.greet()`) we let `walk_path`
    /// descend through every intermediate field — the prefix that gets
    /// fed to `walk_path` is `[o, customer]`, and a successful
    /// `Resolved(Schema("User"))` makes `greet` dispatch against
    /// `User`'s method table.
    ///
    /// Returns `None` when the prefix walk doesn't terminate on a
    /// schema (`UnknownHead` / `UnknownStep` / non-schema final type).
    /// Callers fall through to whatever non-schema dispatch path they
    /// own — no new diagnostic is emitted here.
    fn resolve_method_receiver_prefix(&self, prefix: &[TokenKey]) -> Option<String> {
        debug_assert!(!prefix.is_empty());
        if prefix.len() == 1 {
            let TokenKey::String(head, _, _) = &prefix[0] else {
                return None;
            };
            return self.resolve_method_receiver(head);
        }
        let scope = self.build_type_scope();
        if let infer::PathTailOutcome::Resolved(InferredType::Schema(name)) =
            infer::walk_path(prefix, &scope)
        {
            return Some(name);
        }
        None
    }

    /// Push a `StaticTypeMismatch` when a binary operator is applied to
    /// statically-incompatible operand types (`1 + "hello"`, `true * 3`,
    /// …). Unknown operands (`Any`, `None`) silently pass — runtime
    /// keeps owning the authoritative call.
    fn check_binary_mismatch(
        &mut self,
        node: &Node,
        op: relon_parser::Operator,
        left: &Node,
        right: &Node,
        field_name: Option<&str>,
    ) {
        let scope = self.build_type_scope();
        let Some(lt) = infer_type(left, &scope) else {
            return;
        };
        let Some(rt) = infer_type(right, &scope) else {
            return;
        };
        if !infer::binary_known_invalid(op, &lt, &rt) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
            field: field_name.unwrap_or("_").to_string(),
            expected: format!("{op:?} operands compatible"),
            found: format!("{} {op:?} {}", lt.name(), rt.name()),
            range: span_of(node.range),
        });
    }

    /// Stage 5: try to fold `node` as a literal arithmetic expression.
    /// Pushes `ConstDivisionByZero` or `ConstNumericOverflow` when the
    /// fold trips, and returns `true` so the caller can stop recursing
    /// (avoiding duplicate diagnostics on overlapping subtrees).
    fn check_const_fold(&mut self, node: &Node) -> bool {
        match crate::const_fold::try_fold(node) {
            Err(crate::const_fold::FoldError::DivByZero(range)) => {
                self.tree.diagnostics.push(Diagnostic::ConstDivisionByZero {
                    range: span_of(range),
                });
                true
            }
            Err(crate::const_fold::FoldError::Overflow { op, range }) => {
                self.tree
                    .diagnostics
                    .push(Diagnostic::ConstNumericOverflow {
                        op: format!("{op:?}"),
                        range: span_of(range),
                    });
                true
            }
            // Whole subtree folds cleanly to a constant — nothing to
            // diagnose. Fully-folded nodes still get walked normally
            // (caller decides) so any sibling diagnostics stay live.
            Ok(_) => false,
        }
    }

    /// v1.3: under strict mode, an FnCall whose name resolves *only*
    /// through the host's native fn allowlist (no static signature
    /// describing its return) leaks `Any` into the surrounding type
    /// flow. Surface a `NativeFnSignatureMissing` so the user adds a
    /// signature or stops calling the unknown native fn.
    fn check_strict_fn_call(&mut self, node: &relon_parser::Node, path: &[TokenKey]) {
        if !self.tree.strict_mode {
            return;
        }
        // Pipe RHS: same suppression as `check_fn_call` — the static
        // arity is intentionally one short and we already validated the
        // pipe's operands.
        if self.pipe_target_calls.contains(&node.id) {
            return;
        }
        let TokenKey::String(name, _, _) = path.first().unwrap_or(&TokenKey::Dummy) else {
            return;
        };
        // Single-segment only — multi-segment paths go through
        // `resolve_call_signature`, which already returns `None` for
        // anything we can't classify (and that's covered by the host
        // signature check above).
        if path.len() != 1 {
            return;
        }
        // If we have a static signature, the return type is known —
        // strict mode is satisfied.
        if lookup_signature(name, self.tree, &self.tree.host_fn_signatures).is_some() {
            return;
        }
        // The fn is *registered* (allowlisted) but lacks a signature.
        // That's the precise "native fn whose return we can't see"
        // shape strict mode forbids.
        if self.tree.host_fn_names.contains(name) {
            self.tree
                .diagnostics
                .push(Diagnostic::NativeFnSignatureMissing {
                    fn_name: name.clone(),
                    range: span_of(node.range),
                });
        }
    }

    /// v1.3: validate every entry of a dict literal for typed-spread,
    /// typed-dynamic-key, and DuplicateField rules. The latter fires
    /// regardless of strict mode (the language always treated spread-
    /// induced collisions as ambiguous; v1.2 silently picked one);
    /// the typed-hint rules only fire under strict mode because a
    /// non-strict file is allowed to defer to runtime.
    fn check_dict_v1_3(&mut self, pairs: &[(TokenKey, relon_parser::Node)]) {
        let strict = self.tree.strict_mode;
        // Track every named key declared so far so we can report the
        // first collision against the dict's named entries. Spread-
        // contributed keys are also folded in via the schema index when
        // a typed spread's source schema is statically known.
        let mut named: HashSet<String> = HashSet::new();
        // v1.8+ fix: dynamic-key inner expressions whose typehint we
        // need to validate after the per-pair loop completes (the loop
        // body holds an immutable borrow of `pairs`, so we can't call
        // `&mut self` methods like `check_typed_binding` directly).
        let mut post_dynkey_checks: Vec<(TypeNode, relon_parser::Node)> = Vec::new();
        for (key, _) in pairs {
            if let TokenKey::String(name, _, _) = key {
                named.insert(name.clone());
            }
        }
        let mut to_emit: Vec<Diagnostic> = Vec::new();
        // Track keys contributed by earlier spreads so two spreads that
        // overlap also produce DuplicateField.
        let mut spread_keys: HashSet<String> = HashSet::new();
        for (key, value) in pairs {
            match key {
                TokenKey::Spread(range) => {
                    let has_hint = value.type_hint.is_some();
                    let is_dict_literal = matches!(&*value.expr, Expr::Dict(_));
                    // Resolve a Variable / Reference head to its
                    // sibling target; if that target is itself a
                    // typed binding to a known schema we treat the
                    // spread as having a derivable shape.
                    let derived_schema = self.spread_source_schema(value);
                    let resolves_to_schema = derived_schema.is_some();
                    // v1.4: a `Dict<K, V>`-typed spread source is
                    // also acceptable under strict mode — the value
                    // type is fully classified even though the keys
                    // are dynamic. Detect via the inference walker
                    // so path chains and FnCall returns work the
                    // same as the schema case.
                    let resolves_to_dict = self.spread_source_is_dict(value);
                    let is_spreadable_shape =
                        is_dict_literal || has_hint || resolves_to_schema || resolves_to_dict;

                    // Cross-mode: if the source has a known static
                    // type but that type isn't dict-shaped (Int,
                    // List<T>, Bool, ...) the program is wrong in
                    // every mode — no `<T>` hint can rescue it.
                    if !is_spreadable_shape {
                        if let Some(known_ty) = self.spread_source_known_non_dict(value) {
                            to_emit.push(Diagnostic::NonSpreadableSource {
                                source_type: known_ty,
                                range: span_of(*range),
                            });
                        } else if strict {
                            // Strict-only: the source's type is
                            // genuinely unknown — adding a hint or
                            // typing the binding is the literal fix.
                            to_emit.push(Diagnostic::SpreadSourceTypeUnknown {
                                range: span_of(*range),
                            });
                        }
                    }
                    // Cross-mode: an explicit `<Schema>` hint (or a
                    // resolved-source schema name) that isn't in the
                    // workspace's schema set is broken regardless of
                    // strict mode — the runtime would error too.
                    if let Some(name) = &derived_schema {
                        if !self.schema_known(name) {
                            to_emit.push(Diagnostic::UnresolvedSchema {
                                name: name.clone(),
                                range: span_of(*range),
                            });
                        }
                    }
                    // DuplicateField: collect the spread's contributed
                    // keys when we can derive them statically (typed
                    // spread → schema index, dict literal → its keys).
                    let contributed = self.spread_contributed_keys(value);
                    for k in &contributed {
                        if named.contains(k) || spread_keys.contains(k) {
                            to_emit.push(Diagnostic::DuplicateField {
                                field: k.clone(),
                                range: span_of(*range),
                            });
                        }
                    }
                    spread_keys.extend(contributed);
                }
                TokenKey::Dynamic(inner, _) => {
                    if strict && inner.type_hint.is_none() {
                        to_emit.push(Diagnostic::DynamicKeyTypeUnknown {
                            range: span_of(inner.range),
                        });
                    }
                    // v1.8+ fix: when a typehint *is* present, validate
                    // the key expression against it. Pre-fix the dict
                    // walker only visited `value`, never `inner`, so
                    // `[<String> 1]: v` parsed as well-typed and the
                    // mismatch only surfaced at runtime. Defer to
                    // `check_typed_binding`'s subsumption machinery so
                    // the diagnostic shape matches the rest of the
                    // typed-binding cases.
                    if let Some(hint) = &inner.type_hint {
                        let hint_clone = hint.clone();
                        let inner_clone = inner.clone();
                        // Avoid double-reporting on already-bogus types
                        // — `ban_unsafe_types`/UnknownTypeName already
                        // owns malformed-hint reports.
                        post_dynkey_checks.push((hint_clone, inner_clone));
                    }
                }
                _ => {}
            }
        }
        self.tree.diagnostics.extend(to_emit);
        // v1.8+ fix: now that the immutable borrow of `pairs` is
        // released, validate every typed dynamic-key inner expression
        // against its hint. We share `check_typed_binding`'s machinery
        // so the diagnostic shape matches every other typed-binding
        // case (literal-vs-hint mismatch, schema field check, etc.).
        for (hint, inner) in post_dynkey_checks {
            self.check_typed_binding(&hint, &inner, "<dynamic key>");
        }
    }

    /// True when `name` corresponds to a declared schema reachable
    /// from this analysis pass: dict-field `#schema X ...`, root-level
    /// `#schema X Body`, prelude (`Result`, `Option`), or an import
    /// brought in via spread / destructure / alias.
    fn schema_known(&self, name: &str) -> bool {
        if matches!(name, "Result" | "Option") {
            return true;
        }
        if self.schema_index.contains_key(name) {
            return true;
        }
        if self.tree.root_schemas.iter().any(|d| d.name == name) {
            return true;
        }
        if let Some(idx) = &self.tree.workspace_import_index {
            if idx.spread.contains(name)
                || idx.destructured.contains_key(name)
                || idx.aliased.contains_key(name)
            {
                return true;
            }
        }
        false
    }

    /// Try to derive the schema name a spread source references. v1.4
    /// recognises four static cases:
    ///
    /// 1. An inline `<T>` typehint on the spread itself.
    /// 2. A resolved sibling field with an explicit `T:` type binding.
    /// 3. A path expression (`...x.y.z`) whose tail-walk produces a
    ///    `Schema` head — covers `...o.extras` against
    ///    `Order { Extras extras: * }` and friends.
    /// 4. A `FnCall` (`...load_extras()`) whose static signature
    ///    declares a single-segment `Schema` return type.
    ///
    /// Returns `None` when none of those apply, leaving strict mode to
    /// demand an explicit hint.
    fn spread_source_schema(&self, value: &relon_parser::Node) -> Option<String> {
        if let Some(t) = &value.type_hint {
            if t.path.len() == 1 {
                let head = &t.path[0];
                // v1.8+ fix: a builtin head (`Dict`, `List`, `Tuple`,
                // `Closure`, primitives, ...) is *not* a schema name,
                // even when it carries generics. The companion
                // `spread_source_is_dict` covers `Dict<K, V>`-typed
                // spreads; returning the head as a schema name here
                // would push a bogus `UnresolvedSchema("Dict")`.
                if relon_parser::is_builtin_type_name(head) {
                    return None;
                }
                if matches!(head.as_str(), "Result" | "Option") {
                    return None;
                }
                return Some(head.clone());
            }
        }
        // Resolved reference to a sibling whose head carries a type
        // hint we can lift directly. Same as v1.3 — kept for the
        // single-segment `...e` case where path-tail walking would
        // collapse to the head's type anyway.
        if let Some(resolved) = self.tree.references.get(&value.id) {
            if let Some(target) = self.tree.node_index.get(&resolved.target) {
                if let Some(hint) = target.type_hint.as_ref() {
                    if hint.path.len() == 1 {
                        return Some(hint.path[0].clone());
                    }
                }
            }
        }
        // v1.4 case 3: the spread source is a `Variable` / `Reference`
        // path whose tail-walk lands on a `Schema(name)` (a multi-hop
        // chain like `...o.extras` or a single-segment param like
        // `...o`). The walker reuses the same machinery as the
        // expression-level inference engine, so multi-segment chains
        // through schema fields and dict generics work identically.
        if matches!(&*value.expr, Expr::Variable(_) | Expr::Reference { .. }) {
            let scope = self.build_type_scope();
            let path = match &*value.expr {
                Expr::Variable(p) => p.as_slice(),
                Expr::Reference { path, .. } => path.as_slice(),
                _ => unreachable!(),
            };
            if let infer::PathTailOutcome::Resolved(InferredType::Schema(name)) =
                infer::walk_path(path, &scope)
            {
                return Some(name);
            }
        }
        // v1.4 case 4: the spread source is a static FnCall whose
        // signature has a single-segment Schema return type. Generics
        // aren't unified here because spread-source signatures we'd
        // accept under strict mode never carry placeholders today; if
        // that changes, route through `instantiate` like the FnCall
        // arm of `infer_type` does.
        if let Expr::FnCall { path, .. } = &*value.expr {
            if let Some(sig) = self.resolve_call_signature(path) {
                if sig.generics.is_empty() && sig.return_type.path.len() == 1 {
                    return Some(sig.return_type.path[0].clone());
                }
            }
        }
        None
    }

    /// v1.4: companion to [`Self::spread_source_schema`] for sources
    /// whose static type is `Dict<K, V>` rather than a named schema.
    /// Strict mode treats these as fully classified — the value type
    /// is known, even if the keys are dynamic — so they don't need a
    /// `<T>` typehint. Returns `true` when the spread source's static
    /// type is some `Dict<...>` variant.
    /// Return the printable name of the spread source's static type
    /// when that type is **known** and **isn't** dict-shaped — the
    /// signal for `NonSpreadableSource`. Returns `None` for dict /
    /// schema / `Dict<K,V>` sources (the spread is fine) or for
    /// genuinely unknown / `Any` sources (the strict-only
    /// `SpreadSourceTypeUnknown` covers them).
    ///
    /// Only inspects expression forms whose type the inference layer
    /// is confident about today: literal scalars / lists, references
    /// to typed bindings whose declared type is a non-dict primitive,
    /// and path-tail walks resolving to a non-dict / non-schema head.
    fn spread_source_known_non_dict(&self, value: &relon_parser::Node) -> Option<String> {
        // Literal scalars and lists are obvious offenders.
        match &*value.expr {
            Expr::Int(_) => return Some("Int".to_string()),
            Expr::Float(_) => return Some("Float".to_string()),
            Expr::Bool(_) => return Some("Bool".to_string()),
            Expr::String(_) => return Some("String".to_string()),
            Expr::List(_) => return Some("List".to_string()),
            Expr::Null => return Some("Null".to_string()),
            _ => {}
        }
        // Fall through to the inference walker for everything else
        // (binops like `1 + 2`, identifiers, path chains).
        let scope = self.build_type_scope();
        let inferred = infer::infer_type(value, &scope).unwrap_or(InferredType::Any);
        match inferred {
            // Dict-shaped — handled by `spread_source_is_dict`.
            InferredType::Dict(_) | InferredType::Schema(_) => None,
            // Genuinely unknown — `SpreadSourceTypeUnknown` covers it.
            InferredType::Any => None,
            // Anything else has a known, non-spreadable shape.
            InferredType::Null => Some("Null".to_string()),
            InferredType::Bool => Some("Bool".to_string()),
            InferredType::Int => Some("Int".to_string()),
            InferredType::Float => Some("Float".to_string()),
            InferredType::Number => Some("Number".to_string()),
            InferredType::String => Some("String".to_string()),
            InferredType::List(_) => Some("List".to_string()),
            InferredType::Variant(_, _) => Some("Variant".to_string()),
            InferredType::Optional(_) => Some("Optional".to_string()),
            InferredType::Fn(_, _) => Some("Fn".to_string()),
            InferredType::Tuple(_) => Some("Tuple".to_string()),
        }
    }

    fn spread_source_is_dict(&self, value: &relon_parser::Node) -> bool {
        // Inline `<Dict<K, V>>` typehint.
        if let Some(t) = &value.type_hint {
            if t.path.len() == 1 && t.path[0] == "Dict" {
                return true;
            }
        }
        // `...x` where `x` is bound to a sibling whose declared type is
        // some `Dict<...>` — same lift the schema case does, but for
        // the dict head.
        if let Some(resolved) = self.tree.references.get(&value.id) {
            if let Some(target) = self.tree.node_index.get(&resolved.target) {
                if let Some(hint) = target.type_hint.as_ref() {
                    if hint.path.len() == 1 && hint.path[0] == "Dict" {
                        return true;
                    }
                }
            }
        }
        // Path chain (`...o.kv`) or FnCall (`...load_kv()`) whose
        // inference produces a `Dict(_)` — reuse the central walker.
        match &*value.expr {
            Expr::Variable(p) | Expr::Reference { path: p, .. } => {
                let scope = self.build_type_scope();
                if let infer::PathTailOutcome::Resolved(InferredType::Dict(_)) =
                    infer::walk_path(p, &scope)
                {
                    return true;
                }
            }
            Expr::FnCall { path, .. } => {
                if let Some(sig) = self.resolve_call_signature(path) {
                    if sig.return_type.path.len() == 1 && sig.return_type.path[0] == "Dict" {
                        return true;
                    }
                }
            }
            _ => {}
        }
        false
    }

    /// Best-effort enumeration of the keys a spread source statically
    /// contributes. Returns an empty `Vec` when the source is a typed
    /// spread whose schema isn't visible, or a non-literal expression
    /// without a hint — DuplicateField only fires on contributions we
    /// can prove.
    fn spread_contributed_keys(&self, value: &relon_parser::Node) -> Vec<String> {
        let mut out = Vec::new();
        if let Expr::Dict(inner_pairs) = &*value.expr {
            for (k, _) in inner_pairs {
                if let TokenKey::String(name, _, _) = k {
                    out.push(name.clone());
                }
            }
            return out;
        }
        if let Some(name) = self.spread_source_schema(value) {
            if let Some(fields) = self.schema_index.get(&name) {
                out.extend(fields.keys().cloned());
            }
        }
        out
    }

    /// Build a type-scope reflecting the active dict / closure stack so
    /// inference can resolve `Variable(x)` / `&sibling.y` heads to their
    /// static type-hints.
    fn build_type_scope(&self) -> TypeScope<'_> {
        TypeScope {
            locals: HashMap::new(),
            schemas: Some(&self.schema_index),
            frames: self.scope_stack.iter().collect(),
            tree: Some(self.tree),
        }
    }

    /// v1.5 helper: build a type-scope with the supplied closure
    /// parameters seeded into `locals`, so a strict-mode closure-body
    /// inference walks the body in the same scope shape the runtime
    /// would produce when calling the closure. Untyped params land as
    /// `Any` (the strict-mode untyped-param check already pinned
    /// the diagnostic on the closure node, so we don't need to fail
    /// here as well).
    fn build_type_scope_with_closure(&self, params: &[ClosureParam]) -> TypeScope<'_> {
        let mut locals = HashMap::new();
        let imports = self.tree.workspace_import_index.as_ref();
        for p in params {
            let ty = p
                .type_hint
                .as_ref()
                .map(|t| infer::infer_from_type_node_with_imports(t, imports))
                .unwrap_or(InferredType::Any);
            locals.insert(p.name.clone(), ty);
        }
        TypeScope {
            locals,
            schemas: Some(&self.schema_index),
            frames: self.scope_stack.iter().collect(),
            tree: Some(self.tree),
        }
    }

    /// Stage 1.6: report `MatchArmTypeMismatch` when arm bodies
    /// produce statically-incompatible types. We only fire when *every*
    /// non-wildcard arm body is inferrable; if any arm relies on a
    /// runtime-only computation, we punt (runtime keeps owning the
    /// verdict). Wildcard arms are excluded from the join because their
    /// body type is unconstrained by exhaustiveness rules.
    fn check_match_arm_types(
        &mut self,
        match_range: TokenRange,
        scrutinee: &Node,
        arms: &[(Node, Node)],
    ) {
        let scope = self.build_type_scope();
        let mut arm_types: Vec<InferredType> = Vec::new();
        let strict = self.tree.strict_mode;
        for (pat, body) in arms {
            if matches!(pat.expr.as_ref(), Expr::Wildcard) {
                continue;
            }
            // If any non-wildcard arm body is uninferrable, defer to
            // runtime — we'd be guessing.
            let Some(t) = infer_type(body, &scope) else {
                // v1.4: strict mode demands every arm have a static
                // type. Pin the diagnostic on the failing arm body so
                // the user knows where to add an annotation.
                if strict {
                    self.tree
                        .diagnostics
                        .push(Diagnostic::ExpressionTypeUnknown {
                            reason: "match arm body type is not statically derivable".to_string(),
                            range: span_of(body.range),
                        });
                }
                return;
            };
            arm_types.push(t);
        }
        if arm_types.len() < 2 {
            return;
        }
        // Reduce-by-join across all arm types; if the result collapses
        // to `Any` while none of the inputs were `Any`, the arms are
        // statically heterogeneous.
        let any_input_was_any = arm_types.iter().any(|t| matches!(t, InferredType::Any));
        let mut joined = arm_types[0].clone();
        for t in &arm_types[1..] {
            joined = InferredType::join(&joined, t);
        }
        if matches!(joined, InferredType::Any) && !any_input_was_any {
            let enum_name = self.infer_enum_type(scrutinee);
            self.tree
                .diagnostics
                .push(Diagnostic::MatchArmTypeMismatch {
                    enum_name,
                    arm_types: arm_types.iter().map(|t| t.name()).collect(),
                    range: span_of(match_range),
                });
        }
    }

    /// Push a `StaticTypeMismatch` when a closure's body inference
    /// disagrees with its declared `-> Type`. Closures whose body
    /// remains uninferrable (FnCall, dynamic refs) silently pass.
    fn check_closure_return(
        &mut self,
        params: &[relon_parser::ClosureParam],
        body: &Node,
        declared_return: &TypeNode,
        field_name: Option<&str>,
    ) {
        // Build a fresh inference scope: dict frames inherited from
        // the active stack, locals seeded with the closure's typed
        // params (untyped params default to `Any` so they don't gate
        // analysis).
        let mut locals = HashMap::new();
        let imports = self.tree.workspace_import_index.as_ref();
        for param in params {
            let ty = param
                .type_hint
                .as_ref()
                .map(|t| infer::infer_from_type_node_with_imports(t, imports))
                .unwrap_or(InferredType::Any);
            locals.insert(param.name.clone(), ty);
        }
        let scope = TypeScope {
            locals,
            schemas: Some(&self.schema_index),
            frames: self.scope_stack.iter().collect(),
            tree: Some(self.tree),
        };
        let Some(body_ty) = infer_type(body, &scope) else {
            return;
        };
        if body_ty.subsumes_with_imports(declared_return, Some(&self.base_index), imports) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
            field: field_name.unwrap_or("_").to_string(),
            expected: format_type(declared_return),
            found: body_ty.name(),
            range: span_of(body.range),
        });
    }

    /// Conservative exhaustiveness check: only fires when we can statically
    /// determine the matched expression's type to be a sum-type Enum.
    /// Otherwise we silently fall through to the runtime mismatch path.
    fn check_match_exhaustiveness(
        &mut self,
        match_range: TokenRange,
        expr: &Node,
        arms: &[(Node, Node)],
    ) {
        let Some(enum_name) = self.infer_enum_type(expr) else {
            return;
        };
        // Borrow the variant list for the whole arm walk; we route diagnostic
        // pushes through a local buffer so the read-only borrow of
        // `enum_index` doesn't collide with `&mut self.tree.diagnostics`.
        let Some(variants) = self.enum_index.get(&enum_name) else {
            return;
        };

        let mut seen = HashSet::new();
        let mut has_wildcard = false;
        let mut diags: Vec<Diagnostic> = Vec::new();
        for (pat, _) in arms {
            match pat.expr.as_ref() {
                Expr::Wildcard => {
                    has_wildcard = true;
                }
                Expr::Type(t) if t.path.len() == 1 => {
                    let arm_name = &t.path[0];
                    if !variants.contains(arm_name) {
                        let suggestion = closest_variant(arm_name, variants);
                        diags.push(Diagnostic::UnknownVariant {
                            enum_name: enum_name.clone(),
                            variant_name: arm_name.clone(),
                            suggestion,
                            range: span_of(pat.range),
                        });
                        continue;
                    }
                    if !seen.insert(arm_name.clone()) {
                        diags.push(Diagnostic::DuplicateMatchArm {
                            enum_name: enum_name.clone(),
                            variant_name: arm_name.clone(),
                            range: span_of(pat.range),
                        });
                    }
                }
                _ => {}
            }
        }
        if !has_wildcard {
            let missing: Vec<String> = variants
                .iter()
                .filter(|v| !seen.contains(*v))
                .cloned()
                .collect();
            if !missing.is_empty() {
                diags.push(Diagnostic::NonExhaustiveMatch {
                    enum_name,
                    missing_variants: missing,
                    range: span_of(match_range),
                });
            }
        }
        self.tree.diagnostics.extend(diags);
    }

    /// Try to determine the static enum-name of `node`. We only handle
    /// the cases where the analyzer's resolution table already gave us a
    /// stable target NodeId — anything else would require re-walking the
    /// AST, which is exactly what we want to avoid here.
    fn infer_enum_type(&self, node: &Node) -> Option<String> {
        let resolved = self.tree.references.get(&node.id)?;
        let target_node = self.tree.node_index.get(&resolved.target)?.clone();
        // Direct hint on the binding: `Notification x: ...` declared a
        // type that names a sum-type Enum schema.
        if let Some(t) = &target_node.type_hint {
            if t.path.len() == 1 && self.enum_index.contains_key(&t.path[0]) {
                return Some(t.path[0].clone());
            }
        }
        // Fallback: the binding's value is itself a `VariantCtor` —
        // its enum_path[0] is the enum schema head.
        if let Expr::VariantCtor { enum_path, .. } = target_node.expr.as_ref() {
            if !enum_path.is_empty() && self.enum_index.contains_key(&enum_path[0]) {
                return Some(enum_path[0].clone());
            }
        }
        None
    }

    fn check_unresolved_ref(&mut self, node: &Node, base: &RefBase, path: &[TokenKey]) {
        if self.tree.references.contains_key(&node.id) {
            // Head resolved — but multi-segment paths still need a tail
            // walk (Stage 2.6) to catch `obj.b` where `obj` exists but
            // `b` doesn't.
            self.check_path_tail(node, path);
            // v1.4: under strict mode, the tail walk additionally
            // surfaces `UnknownReferenceType` whenever the inference
            // walker can prove a step lands on an opaque value (head
            // is `Any` / a non-schema-non-dict head). Run it even for
            // single-segment paths: the head's resolved target may
            // still carry no static type.
            self.check_strict_path(node, path);
            return;
        }
        // Static analyzer skipped this reference. Decide whether to
        // flag it.
        match base {
            // List-context refs depend on iteration state — never flag.
            RefBase::This | RefBase::Prev | RefBase::Next | RefBase::Index => return,
            _ => {}
        }
        let Some(name) = path_head(path) else { return };
        if self.dynamic_save(&name) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
            name: name.clone(),
            range: span_of(node.range),
        });
        // v1.5: strict mode escalates the head-unresolved case from a
        // warning-level `UnresolvedReference` to an error-level
        // `UnknownReferenceType { path: [name, ...] }` so the
        // evaluator never reaches a runtime-only "name not found"
        // path. The warning still fires for non-strict analyzers / IDE
        // hints; strict callers see the matching error too.
        if self.tree.strict_mode {
            let segs = infer::path_segments(path);
            self.tree
                .diagnostics
                .push(Diagnostic::UnknownReferenceType {
                    name,
                    path: segs,
                    range: span_of(node.range),
                });
        }
    }

    fn check_unresolved_var(&mut self, node: &Node, path: &[TokenKey]) {
        if self.tree.references.contains_key(&node.id) {
            // Same Stage 2.6 tail walk as `check_unresolved_ref`.
            self.check_path_tail(node, path);
            // v1.4 strict: also run the inference-driven tail walk so
            // multi-segment opaque steps (`o.unknown`, `f.x.y`) emit
            // `UnknownReferenceType` rather than silently leaking
            // `Any`.
            self.check_strict_path(node, path);
            return;
        }
        let Some(name) = path_head(path) else { return };
        // Variables also resolve against function names registered
        // by the host (stdlib like `range`, `len`, ...). The analyzer
        // consults the evaluator's hardcoded stdlib name set plus the
        // host-supplied `host_fn_names` (Stage 2.4) before flagging.
        if self.dynamic_save(&name) || self.is_known_fn(&name) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
            name: name.clone(),
            range: span_of(node.range),
        });
        // v1.5: strict-mode escalation, same as `check_unresolved_ref`.
        if self.tree.strict_mode {
            let segs = infer::path_segments(path);
            self.tree
                .diagnostics
                .push(Diagnostic::UnknownReferenceType {
                    name,
                    path: segs,
                    range: span_of(node.range),
                });
        }
    }

    /// Run the inference-driven path walker over `path` and report a
    /// [`Diagnostic::UnknownReferenceType`] whenever a segment can't
    /// be classified. Splits the responsibility:
    ///
    /// * `UnknownStep` (e.g. `o.unknown` where `o: Schema` has no
    ///   `unknown` field, or `int_field.something` descending past a
    ///   leaf) is a **static error** — the analyzer has positive
    ///   knowledge the path is broken. Fired cross-mode.
    /// * `Resolved(Any)` (e.g. path runs into an untyped closure
    ///   parameter whose type the analyzer literally can't see) is a
    ///   **strict-only** finding — the analyzer doesn't *know* the
    ///   path is broken, it just refuses to keep going under strict.
    /// * `UnknownHead` is owned by the resolution-side
    ///   `UnresolvedReference` diagnostic, so we don't report here.
    fn check_strict_path(&mut self, node: &Node, path: &[TokenKey]) {
        let segs = infer::path_segments(path);
        if segs.is_empty() {
            return;
        }
        // v1.5 deduplication: when the path is a single segment whose
        // head names an *untyped closure parameter* on the active
        // scope stack, the `ClosureParamTypeMissing` walker
        // already pinned a diagnostic on the param's declaration. Don't
        // double-fire `UnknownReferenceType` on every body reference
        // — the user already knows which param to annotate.
        if segs.len() == 1 {
            let head = &segs[0];
            for frame in self.scope_stack.iter().rev() {
                if frame.closure_params.contains_key(head)
                    && !frame.closure_param_types.contains_key(head)
                {
                    return;
                }
            }
        }
        let scope = self.build_type_scope();
        match infer::walk_path(path, &scope) {
            infer::PathTailOutcome::Resolved(InferredType::Any) => {
                // Head / mid-walk landed on `Any`. The analyzer can't
                // verify or refute the path — strict mode refuses the
                // leak, non-strict keeps the silent fallback.
                if !self.tree.strict_mode {
                    return;
                }
                let last = segs.last().cloned().unwrap_or_default();
                self.tree
                    .diagnostics
                    .push(Diagnostic::UnknownReferenceType {
                        name: last,
                        path: segs,
                        range: span_of(node.range),
                    });
            }
            infer::PathTailOutcome::Resolved(_) => {
                // Fully classified — both modes are satisfied.
            }
            infer::PathTailOutcome::UnknownStep { at_segment, .. } => {
                // Cross-mode: the walker has positive knowledge that
                // the path is broken. The runtime would fail too.
                let name = segs.get(at_segment).cloned().unwrap_or_default();
                self.tree
                    .diagnostics
                    .push(Diagnostic::UnknownReferenceType {
                        name,
                        path: segs,
                        range: span_of(node.range),
                    });
            }
            infer::PathTailOutcome::UnknownHead => {
                // The head wasn't visible. Resolution-side diagnostics
                // (UnresolvedReference) own the head case; we don't
                // double-report here.
            }
        }
    }

    /// Stage 2.6: walk the rest of a multi-segment `path` (after the
    /// head bound to a known field / param). For each segment, narrow
    /// the running type:
    ///
    /// * `Schema(name)` → segment must be a declared field of that
    ///   schema; otherwise push `UnresolvedReference("obj.field")`.
    /// * `Dict(value_ty)` / `Optional(...)` → continue with the inner
    ///   type. Without per-key info we can't validate the segment, so
    ///   we just walk past it.
    /// * `Any` / FnCall result / unknown / closure-param-without-type
    ///   → silent fall-back (defer to runtime).
    fn check_path_tail(&mut self, node: &Node, path: &[TokenKey]) {
        if path.len() < 2 {
            return;
        }
        let scope = self.build_type_scope();
        let head = match path.first() {
            Some(TokenKey::String(s, _, _)) => s.clone(),
            _ => return,
        };

        // Stage 2.6 fast-path for dict-literal field lookup: if the
        // head resolves to a sibling whose value is a `Dict` literal
        // *without* an explicit schema type hint, we can validate the
        // first tail segment directly against the literal's keys. This
        // catches `{ obj: { a: 1 }, x: obj.b }` where `Dict(Any)` would
        // otherwise be too coarse to flag the missing `b`.
        if let Some(target_node) = self.lookup_field_node(&head) {
            if target_node.type_hint.is_none() {
                if let Expr::Dict(inner_pairs) = &*target_node.expr {
                    if let Some(TokenKey::String(seg_name, _, _)) = path.get(1) {
                        let has_key = inner_pairs
                            .iter()
                            .any(|(k, _)| matches!(k, TokenKey::String(n, _, _) if n == seg_name));
                        // Any non-dict-literal spread inside the inner
                        // dict makes the key-set dynamic — we can't say
                        // for sure that `seg_name` is missing.
                        let has_dynamic = inner_pairs.iter().any(|(k, v)| {
                            matches!(k, TokenKey::Spread(_)) && !matches!(&*v.expr, Expr::Dict(_))
                        });
                        if !has_key && !has_dynamic {
                            self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
                                name: format!("{head}.{seg_name}"),
                                range: span_of(node.range),
                            });
                            return;
                        }
                    }
                }
            }
        }

        let Some(mut current) = scope.lookup(&head) else {
            return;
        };
        let mut accumulated = head.clone();
        // Walk path[1..]: each String segment must exist on the running
        // type (or we punt). We strip Optional wrappers as we walk so
        // `T? . x` is checked against `T`'s field set.
        for seg in &path[1..] {
            let TokenKey::String(name, _, _) = seg else {
                return;
            };
            current = match current {
                InferredType::Optional(inner) => *inner,
                other => other,
            };
            match &current {
                InferredType::Any => return,
                InferredType::Schema(schema_name) => {
                    let Some(fields) = self.schema_index.get(schema_name) else {
                        // Unknown schema body — runtime owns the verdict.
                        return;
                    };
                    let Some(field_ty) = fields.get(name) else {
                        let full = format!("{accumulated}.{name}");
                        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
                            name: full,
                            range: span_of(node.range),
                        });
                        return;
                    };
                    accumulated = format!("{accumulated}.{name}");
                    current = infer::infer_from_type_node(field_ty);
                }
                InferredType::Dict(value_ty) => {
                    // Homogeneous dict — every key has type `value_ty`,
                    // so the segment is structurally fine. Continue
                    // walking with the value type.
                    accumulated = format!("{accumulated}.{name}");
                    current = (**value_ty).clone();
                }
                _ => return, // Other shapes: defer to runtime.
            }
        }
    }

    /// True when `name` is a name the host or evaluator has registered
    /// as a callable / value — either the hardcoded stdlib set
    /// (`range`, `len`, …), a host-registered native fn whose name
    /// reached the analyzer via [`crate::AnalyzeOptions::host_fn_names`],
    /// or (v1.1) a closure exposed by a cross-module `#import` (spread +
    /// destructure forms; alias forms only contribute `alias.method`
    /// paths which `check_unresolved_*` doesn't see as a single name).
    fn is_known_fn(&self, name: &str) -> bool {
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
    fn lookup_field_node(&self, name: &str) -> Option<Arc<relon_parser::Node>> {
        for frame in self.scope_stack.iter().rev() {
            if let Some(id) = frame.fields.get(name).copied() {
                return self.tree.node_index.get(&id).cloned();
            }
        }
        None
    }

    /// True if any frame on the active scope chain has a dynamic
    /// spread or a closure param matching `name`.
    fn dynamic_save(&self, name: &str) -> bool {
        self.scope_stack
            .iter()
            .rev()
            .any(|frame| frame.might_dynamically_bind(name))
    }

    /// Validate that `value` plausibly satisfies `expected`. Only
    /// fires when the value's shape is statically classifiable; calls
    /// to functions, references, etc. are deferred to the runtime
    /// check.
    fn check_typed_binding(&mut self, expected: &TypeNode, value: &Node, field_name: &str) {
        // Run the inference engine against the active scope so
        // `Variable`/`Reference` heads reach back to their dict
        // siblings and pick up the declared type-hint. Falls back to
        // the legacy name-string path for the diagnostic shape.
        let scope = self.build_type_scope();
        let inferred = infer_type(value, &scope);
        if let Some(t) = &inferred {
            if t.subsumes_with_imports(
                expected,
                Some(&self.base_index),
                self.tree.workspace_import_index.as_ref(),
            ) {
                self.check_generics(expected, value, field_name);
                return;
            }
            // v1.1: when the failure is at the *element* level of a
            // matching outer shape (`List<...>` against `List<...>`,
            // `Dict<...>` against `Dict<...>`) and the value is a
            // literal, defer to the structural `check_generics`
            // walker so the diagnostic carries the precise inner
            // path (`matrix[1][0]: Int / String`) instead of the
            // coarser `matrix[1]: List<Int> / List<String>`. The
            // outer head must still match — an Int landing in a
            // List slot stays a coarse mismatch.
            if same_outer_container(t, expected)
                && matches!(&*value.expr, Expr::List(_) | Expr::Dict(_))
            {
                self.check_generics(expected, value, field_name);
                return;
            }
            self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
                field: field_name.to_string(),
                expected: format_type(expected),
                found: t.name(),
                range: span_of(value.range),
            });
            return;
        }

        // If we couldn't infer a single static type, check if it's a
        // structured expression whose branches we can partially validate.
        if let Expr::Ternary { then, els, .. } = &*value.expr {
            self.check_typed_binding(expected, then, field_name);
            self.check_typed_binding(expected, els, field_name);
            return;
        }

        // v1.4 / v1.5: under strict mode, a typed binding whose value
        // can't produce *any* inferred type leaks `Any` into the
        // typed slot — strict mode demands a derivable type, so emit
        // `ExpressionTypeUnknown` describing the precise reason. v1.5 made
        // comprehensions / where / spread / closures all inferable
        // out-of-the-box, so the only paths that still hit this branch
        // are FnCall without a signature and a handful of edge cases
        // (e.g. unary on a leaf type).
        if self.tree.strict_mode {
            let reason = match &*value.expr {
                Expr::FnCall { path, .. } => match path.first() {
                    Some(TokenKey::String(name, _, _)) => {
                        format!("call to `{name}` has no static return type")
                    }
                    _ => "call result has no static return type".to_string(),
                },
                _ => "value type is not statically derivable".to_string(),
            };
            self.tree
                .diagnostics
                .push(Diagnostic::ExpressionTypeUnknown {
                    reason,
                    range: span_of(value.range),
                });
        }
    }

    /// Recursively check the contents of List and Dict literals against
    /// expected generic parameters.
    fn check_generics(&mut self, expected: &TypeNode, value: &Node, field_name: &str) {
        if expected.generics.is_empty() && expected.path != vec!["Tuple"] {
            return;
        }
        // v1.8 C3: variant-constructor literal landing in a generic
        // sum-type slot. Look up the variant's declared field types,
        // substitute the slot's generic args (`Result<Int, String>` →
        // `T -> Int, E -> String`), then recurse into each body field.
        if expected.path.len() == 1 {
            if let Expr::VariantCtor {
                enum_path,
                variant,
                body,
            } = &*value.expr
            {
                let slot_name = &expected.path[0];
                if enum_path.first().map(|s| s.as_str()) == Some(slot_name.as_str()) {
                    if let Some(variants) = self.variant_field_index.get(slot_name).cloned() {
                        if let Some((generic_params, fields)) = variants.get(variant).cloned() {
                            let mut subst = HashMap::new();
                            for (i, gname) in generic_params.iter().enumerate() {
                                if let Some(arg) = expected.generics.get(i) {
                                    subst.insert(gname.clone(), arg.clone());
                                }
                            }
                            if let Expr::Dict(pairs) = &*body.expr {
                                for (key, inner) in pairs {
                                    if let TokenKey::String(k, _, _) = key {
                                        if let Some(field_ty) = fields.get(k) {
                                            let resolved =
                                                substitute_generics_in_typenode(field_ty, &subst);
                                            self.check_typed_binding(
                                                &resolved,
                                                inner,
                                                &format!("{field_name}.{k}"),
                                            );
                                        }
                                    }
                                }
                            }
                            return;
                        }
                    }
                }
            }
        }
        match &*value.expr {
            Expr::List(items) => {
                if expected.path == vec!["List"] && expected.generics.len() == 1 {
                    let inner_expected = &expected.generics[0];
                    for (i, item) in items.iter().enumerate() {
                        self.check_typed_binding(
                            inner_expected,
                            item,
                            &format!("{}[{}]", field_name, i),
                        );
                    }
                }
                // v1.7: list literal landing in a tuple-typed slot.
                // Validate arity first; on mismatch emit a single
                // top-level diagnostic with an arity-flavored message.
                // On arity match, recurse into each position with the
                // declared positional type.
                if expected.path == vec!["Tuple"] {
                    if items.len() != expected.generics.len() {
                        self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
                            field: field_name.to_string(),
                            expected: format_type(expected),
                            found: format!("tuple of {} element(s)", items.len()),
                            range: span_of(value.range),
                        });
                        return;
                    }
                    for (i, (item, slot)) in items.iter().zip(expected.generics.iter()).enumerate()
                    {
                        self.check_typed_binding(slot, item, &format!("{}[{}]", field_name, i));
                    }
                }
            }
            // Only the common `Dict<String, V>` shape is descended into;
            // other key types are deferred to the runtime checker.
            Expr::Dict(pairs)
                if expected.path == vec!["Dict"]
                    && expected.generics.len() == 2
                    && expected.generics[0].path == vec!["String"] =>
            {
                let inner_expected = &expected.generics[1];
                for (key, val) in pairs {
                    if let TokenKey::String(k, _, _) = key {
                        self.check_typed_binding(
                            inner_expected,
                            val,
                            &format!("{}.{}", field_name, k),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// When `expected` names a custom schema and `value` is a literal
    /// dict, walk the dict's fields and report any that disagree with
    /// the schema's declared field types.
    fn check_against_custom_schema(&mut self, expected: &TypeNode, value: &Node) {
        if expected.path.len() != 1 {
            return;
        }
        let schema_name = &expected.path[0];
        let Expr::Dict(pairs) = &*value.expr else {
            return;
        };
        // Collect the (field_name, expected_type) lookups up front so
        // `check_typed_binding`'s `&mut self.tree` borrow doesn't conflict
        // with the read of `self.schema_index`. Stay lazy: only clone the
        // TypeNode for fields we'll actually check, instead of cloning the
        // whole `field_types` HashMap once per Dict.
        //
        // v1.8+ fix (issue 4): when `expected` carries generic args
        // (`Box<Int>`), substitute the schema's generic param names
        // with the supplied args before recursing — otherwise a field
        // declared as `T value: *` checks the value against the
        // literal type `T` and reports a static mismatch. We pull the
        // schema's generic param names from `tree.schemas` /
        // `root_schemas` (the schema_index only carries field types,
        // not the param list).
        let subst_map = self.build_generic_subst(schema_name, expected);
        let mut to_check: Vec<(String, TypeNode, &Node)> = Vec::new();
        if let Some(field_types) = self.schema_index.get(schema_name) {
            for (key, inner) in pairs {
                if let TokenKey::String(field_name, _, _) = key {
                    if let Some(field_type) = field_types.get(field_name) {
                        let substituted = if subst_map.is_empty() {
                            field_type.clone()
                        } else {
                            substitute_generics_in_typenode(field_type, &subst_map)
                        };
                        to_check.push((field_name.clone(), substituted, inner));
                    }
                }
            }
        }
        for (field_name, field_type, inner) in to_check {
            self.check_typed_binding(&field_type, inner, &field_name);
        }
    }

    /// v1.8+ fix (issue 4): collect the `param_name → arg_TypeNode`
    /// substitution implied by `expected` (e.g. `Box<Int>` against
    /// `#schema Box<T> { ... }` produces `{T → Int}`). Returns an
    /// empty map when the schema isn't generic or no `<...>` was
    /// supplied.
    fn build_generic_subst(
        &self,
        schema_name: &str,
        expected: &TypeNode,
    ) -> HashMap<String, TypeNode> {
        let mut out = HashMap::new();
        if expected.generics.is_empty() {
            return out;
        }
        // Locate the schema's declared generic param names. Both
        // dict-form (`tree.schemas`) and root-form (`tree.root_schemas`)
        // can declare generics; check both.
        let params: Vec<String> = self
            .tree
            .schemas
            .values()
            .find(|def| def.name.as_deref() == Some(schema_name))
            .map(|def| def.generics.clone())
            .or_else(|| {
                self.tree
                    .root_schemas
                    .iter()
                    .find(|d| d.name == schema_name)
                    .map(|d| d.generics.clone())
            })
            .unwrap_or_default();
        for (i, p) in params.iter().enumerate() {
            if let Some(arg) = expected.generics.get(i) {
                out.insert(p.clone(), arg.clone());
            }
        }
        out
    }
}

// `static_type_of` / `matches_expected` were the pre-Stage-1.2
// String-name inference helpers. They've been fully replaced by the
// `infer::infer_type` engine and `InferredType::subsumes`, and the
// last in-tree caller migrated in Stage 1.4. The legacy helpers are
// removed here; consumers that need the old name-string view can
// build it from `InferredType::name`.

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
fn param_is_polymorphic(
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
fn required_and_max(sig: &FnSignature) -> (usize, usize) {
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
fn extract_closure_signature(
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
fn same_outer_container(inferred: &InferredType, expected: &TypeNode) -> bool {
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
fn closest_variant(target: &str, candidates: &[String]) -> Option<String> {
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
pub(crate) fn stdlib_registered_names() -> &'static [&'static str] {
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

fn stdlib_names() -> &'static std::collections::HashSet<&'static str> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    fn analyze_str(src: &str) -> AnalyzedTree {
        let node = parse_document(src).unwrap();
        analyze(&node)
    }

    #[test]
    fn flags_unresolved_sibling_reference() {
        let tree = analyze_str(r#"{ a: 1, b: &sibling.missing }"#);
        let warnings: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "missing"),
            )
            .collect();
        assert_eq!(warnings.len(), 1, "{:?}", tree.diagnostics);
    }

    #[test]
    fn does_not_flag_dynamic_spread() {
        // `merged` has a spread, so a sibling reference inside it
        // can plausibly be saved by a key from `base`.
        let tree = analyze_str(
            r#"{
                base: { x: 1 },
                merged: { ...&sibling.base, hint: x }
            }"#,
        );
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", unresolved);
    }

    #[test]
    fn does_not_flag_closure_param() {
        let tree = analyze_str(r#"{ helper(arg): arg + 1 }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", unresolved);
    }

    #[test]
    fn flags_static_type_mismatch_on_typed_field() {
        let tree = analyze_str(r#"{ Int port: "8080" }"#);
        assert!(
            tree.diagnostics.iter().any(
                |d| matches!(d, Diagnostic::StaticTypeMismatch { expected, found, .. }
                    if expected == "Int" && found == "String")
            ),
            "{:?}",
            tree.diagnostics
        );
    }

    #[test]
    fn allows_optional_null() {
        let tree = analyze_str(r#"{ Int? port: null }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert!(mismatches.is_empty(), "{:?}", mismatches);
    }

    #[test]
    fn flags_mismatch_inside_custom_schema_binding() {
        let tree = analyze_str(
            r#"{
                #schema User { String name: *, Int age: * },
                User alice: { name: "A", age: "thirty" }
            }"#,
        );
        assert!(
            tree.diagnostics.iter().any(
                |d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "age")
            ),
            "{:?}",
            tree.diagnostics
        );
    }

    #[test]
    fn flags_non_exhaustive_match_on_sum_enum() {
        let tree = analyze_str(
            r#"{
                #schema N Enum<A { x: Int }, B { y: Int }, C>,
                N v: N.A { x: 1 },
                out: v match {
                    A: 1,
                    B: 2
                }
            }"#,
        );
        let nx: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::NonExhaustiveMatch { .. }))
            .collect();
        assert_eq!(nx.len(), 1, "{:?}", tree.diagnostics);
        if let Diagnostic::NonExhaustiveMatch {
            enum_name,
            missing_variants,
            ..
        } = nx[0]
        {
            assert_eq!(enum_name, "N");
            assert_eq!(missing_variants, &vec!["C".to_string()]);
        } else {
            panic!()
        }
    }

    #[test]
    fn flags_unknown_variant_with_did_you_mean() {
        let tree = analyze_str(
            r#"{
                #schema N Enum<Email { x: Int }, SMS { y: Int }>,
                N v: N.Email { x: 1 },
                out: v match {
                    EMail: 1,
                    SMS: 2
                }
            }"#,
        );
        let unknown: Vec<_> = tree
            .diagnostics
            .iter()
            .filter_map(|d| match d {
                Diagnostic::UnknownVariant {
                    variant_name,
                    suggestion,
                    ..
                } => Some((variant_name.clone(), suggestion.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(unknown.len(), 1, "{:?}", tree.diagnostics);
        assert_eq!(unknown[0].0, "EMail");
        assert_eq!(unknown[0].1.as_deref(), Some("Email"));
    }

    #[test]
    fn flags_duplicate_match_arm() {
        let tree = analyze_str(
            r#"{
                #schema N Enum<A { x: Int }, B { y: Int }>,
                N v: N.A { x: 1 },
                out: v match {
                    A: 1,
                    A: 2,
                    B: 3
                }
            }"#,
        );
        assert!(
            tree.diagnostics
                .iter()
                .any(|d| matches!(d, Diagnostic::DuplicateMatchArm { variant_name, .. } if variant_name == "A")),
            "{:?}",
            tree.diagnostics
        );
    }

    #[test]
    fn wildcard_arm_satisfies_exhaustiveness() {
        let tree = analyze_str(
            r#"{
                #schema N Enum<A { x: Int }, B { y: Int }, C>,
                N v: N.A { x: 1 },
                out: v match {
                    A: 1,
                    *: 9
                }
            }"#,
        );
        let nx: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::NonExhaustiveMatch { .. }))
            .collect();
        assert!(nx.is_empty(), "{:?}", tree.diagnostics);
    }

    #[test]
    fn skips_exhaustiveness_when_type_uninferrable() {
        // `mystery` has no type hint and isn't a variant constructor
        // — the analyzer can't statically determine its enum, so no
        // exhaustiveness diagnostic should fire.
        let tree = analyze_str(
            r#"{
                #schema N Enum<A { x: Int }, B { y: Int }>,
                mystery: 42,
                out: mystery match {
                    A: 1
                }
            }"#,
        );
        let nx: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::NonExhaustiveMatch { .. }))
            .collect();
        assert!(nx.is_empty(), "{:?}", tree.diagnostics);
    }

    #[test]
    fn flags_nested_list_mismatch() {
        let tree = analyze_str(r#"{ List<Int> items: [1, "two", 3] }"#);
        assert!(
            tree.diagnostics.iter().any(|d| matches!(
                d,
                Diagnostic::StaticTypeMismatch {
                    field,
                    expected,
                    found,
                    ..
                } if field == "items[1]" && expected == "Int" && found == "String"
            )),
            "{:?}",
            tree.diagnostics
        );
    }

    #[test]
    fn flags_nested_dict_mismatch() {
        let tree = analyze_str(r#"{ Dict<String, Int> scores: { math: 100, art: "A" } }"#);
        assert!(
            tree.diagnostics.iter().any(|d| matches!(
                d,
                Diagnostic::StaticTypeMismatch {
                    field,
                    expected,
                    found,
                    ..
                } if field == "scores.art" && expected == "Int" && found == "String"
            )),
            "{:?}",
            tree.diagnostics
        );
    }

    #[test]
    fn infers_binary_expression_types() {
        let tree = analyze_str(
            r#"{
                Int a: 1 + 2,
                Float b: 1 + 2.0,
                String c: "a" + "b",
                Bool d: 1 == 1,
                // These should fail
                Int e: 1.0 + 2.0,
                String f: 1 + 2
            }"#,
        );
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        // e and f should mismatch
        assert_eq!(mismatches.len(), 2, "{:?}", mismatches);
        assert!(mismatches
            .iter()
            .any(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "e")));
        assert!(mismatches
            .iter()
            .any(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "f")));
    }

    #[test]
    fn handles_ternary_type_inference() {
        let tree = analyze_str(
            r#"{
                Int a: true ? 1 : 2,
                Float b: true ? 1 : 2.2,
                // This should fail (heterogeneous)
                Int c: true ? 1 : "2"
            }"#,
        );
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", mismatches);
        assert!(mismatches
            .iter()
            .any(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "c")));
    }

    #[test]
    fn recursive_list_check() {
        let tree = analyze_str(r#"{ List<List<Int>> matrix: [[1], ["two"]] }"#);
        assert!(
            tree.diagnostics.iter().any(|d| matches!(
                d,
                Diagnostic::StaticTypeMismatch {
                    field,
                    expected,
                    found,
                    ..
                } if field == "matrix[1][0]" && expected == "Int" && found == "String"
            )),
            "{:?}",
            tree.diagnostics
        );
    }

    /// Stage 1.3: a binary operator applied to incompatible operands
    /// (Int + String) is reported even when no type hint is in play —
    /// the slot is `Int x:` so the binding line forces an explicit
    /// classification of the value expression.
    #[test]
    fn flags_binary_int_plus_string() {
        let tree = analyze_str(r#"{ Int x: 1 + "hello" }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        // Should have at least one diagnostic — possibly two (one for
        // the binary itself, one for the slot binding).
        assert!(!mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.3 reverse: `Any` slot tolerates the same expression
    /// without producing the binary mismatch (the binding side accepts
    /// it, but the binary itself remains a known-bad combination —
    /// so we still expect the binary diagnostic). Encodes the rule
    /// "the typed binding is happy, but the binary is still wrong".
    #[test]
    fn binary_mismatch_independent_of_slot_type() {
        let tree = analyze_str(r#"{ Any x: 1 + "hello" }"#);
        // The slot accepts Any, so no slot-level mismatch — but the
        // binary itself is still ill-typed.
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert!(!mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.3: untyped slots over a known-bad binary still report —
    /// the binary itself is the offender, regardless of the field.
    #[test]
    fn flags_bare_bool_arithmetic() {
        let tree = analyze_str(r#"{ x: true + 1 }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 1.4 forward: a sibling reference to a typed field carries
    /// the field's declared type back to the binding site. Slots
    /// declaring `Int y` over a `String x` reference should mismatch.
    #[test]
    fn flags_reference_to_typed_sibling() {
        let tree = analyze_str(
            r#"{
                String x: "hello",
                Int y: x
            }"#,
        );
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "y"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 1.4 reverse: when the referenced sibling is itself a fn
    /// call to a name the analyzer can't resolve to a static signature,
    /// stay silent — runtime keeps owning the verdict. (Stage 3 added
    /// signature lookup for stdlib fns like `range`, so we use an
    /// unknown name here to preserve the original silent-on-fncall
    /// invariant.)
    #[test]
    fn does_not_flag_fncall_sibling_reference() {
        let tree = analyze_str(
            r#"{
                xs: dynamic_unknown_fn(),
                Int y: xs
            }"#,
        );
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "y"))
            .collect();
        assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.5 forward: closure body type disagrees with the
    /// declared `-> Type`. The dict-method shorthand `Type key(params): body`
    /// desugars to a closure with `return_type = Type`.
    #[test]
    fn flags_closure_return_type_mismatch() {
        let tree = analyze_str(r#"{ Int helper(Int x): x + "y" }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert!(!mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.5 reverse: untyped param + no return annotation =
    /// silent. Body type inference defaults to `Any` because of the
    /// untyped param, so we have nothing to compare against.
    #[test]
    fn does_not_flag_untyped_closure() {
        let tree = analyze_str(r#"{ f(x): x + 1 }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.6 forward: match arms returning unrelated types
    /// (`Int` vs `String`) collapse to `Any` — flag.
    #[test]
    fn flags_match_arm_type_mismatch() {
        let tree = analyze_str(
            r#"{
                #schema N Enum<A { x: Int }, B { y: Int }>,
                N v: N.A { x: 1 },
                out: v match {
                    A: 1,
                    B: "two"
                }
            }"#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MatchArmTypeMismatch { .. }))
            .collect();
        assert_eq!(mm.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 1.6 reverse: arms returning the same type don't trip the
    /// join collapse — silent.
    #[test]
    fn does_not_flag_homogeneous_match_arms() {
        let tree = analyze_str(
            r#"{
                #schema N Enum<A { x: Int }, B { y: Int }>,
                N v: N.A { x: 1 },
                out: v match {
                    A: 1,
                    B: 2
                }
            }"#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::MatchArmTypeMismatch { .. }))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.9 #9: stdlib FnCalls don't carry a static signature in
    /// the analyzer, so a bare assignment from `range(...)` must stay
    /// silent — no spurious mismatch even though the slot is untyped.
    #[test]
    fn fncall_assignment_is_silent() {
        let tree = analyze_str(r#"{ x: range(0, 10) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.9 #11: `Any` declared slot is silent on the slot side.
    /// The binary itself is still ill-typed, so we expect exactly one
    /// diagnostic (from the binary check), never one from the slot.
    #[test]
    fn any_slot_does_not_add_slot_level_mismatch() {
        let tree = analyze_str(r#"{ Any x: 1 + "y" }"#);
        let slot_mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::StaticTypeMismatch { field, expected, .. }
                        if field == "x" && expected == "Any"
                )
            })
            .collect();
        assert!(slot_mm.is_empty(), "{:?}", tree.diagnostics);
        // There IS one diagnostic — for the binary itself.
        let binary_mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert!(!binary_mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.9 #12 (consistency): when the analyzer reports a
    /// `StaticTypeMismatch`, `tree.has_errors()` flips to true so the
    /// evaluator's facade refuses to run. This pins the gating
    /// contract that Stage 1.1 enabled.
    #[test]
    fn static_type_mismatch_marks_tree_as_errored() {
        let tree = analyze_str(r#"{ Int x: "hello" }"#);
        assert!(tree.has_errors(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.9 #10: a sibling reference to an unknown name flags
    /// `UnresolvedReference` (a warning) but never a spurious
    /// `StaticTypeMismatch` — runtime owns whether it eventually
    /// resolves through a dynamic frame.
    #[test]
    fn unresolved_sibling_does_not_static_mismatch() {
        let tree = analyze_str(r#"{ x: &sibling.unknown }"#);
        let typ: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .collect();
        assert!(typ.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.8 forward: a schema field declares a type whose head
    /// isn't a builtin or a declared schema — flag `UnknownTypeName`.
    #[test]
    fn schema_field_unknown_type_flagged() {
        let tree = analyze_str(
            r#"{
                #schema A { B b: * }
            }"#,
        );
        let unknown: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnknownTypeName { name, .. } if name == "B"))
            .collect();
        assert!(!unknown.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.8 reverse: a declared schema name as a field type stays
    /// silent.
    #[test]
    fn schema_field_known_type_silent() {
        let tree = analyze_str(
            r#"{
                #schema B { Int n: * },
                #schema A { B b: * }
            }"#,
        );
        let unknown: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnknownTypeName { .. }))
            .collect();
        assert!(unknown.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.8: `{ ...{ a: 1, b: 2 }, x: c }` — `c` isn't merged in
    /// from the spread (only `a` and `b` are), so it must flag.
    #[test]
    fn spread_then_unresolved_sibling() {
        let tree = analyze_str(r#"{ ...{a: 1, b: 2}, x: c }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "c"))
            .collect();
        assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 2.7 forward: a function call to a name that isn't bound
    /// to any sibling, closure param, stdlib, or host fn must surface
    /// as `UnresolvedReference`.
    #[test]
    fn fncall_unknown_name_flagged() {
        let tree = analyze_str(r#"{ x: undef_fn() }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "undef_fn"),
            )
            .collect();
        assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 2.7 reverse: stdlib names like `range` are silent.
    #[test]
    fn fncall_stdlib_silent() {
        let tree = analyze_str(r#"{ x: range(0, 10) }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.7 reverse: a sibling-bound closure used as a callee
    /// stays silent.
    #[test]
    fn fncall_sibling_closure_silent() {
        let tree = analyze_str(r#"{ helper(): 1, x: helper() }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.6 forward: a multi-segment path whose tail names a key
    /// missing from the bound dict literal flags `UnresolvedReference`.
    #[test]
    fn dot_path_dict_literal_missing_key_flagged() {
        let tree = analyze_str(r#"{ obj: { a: 1 }, x: obj.b }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "obj.b"),
            )
            .collect();
        assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 2.6 forward: same idea for a typed schema binding — `u.bogus`
    /// where `bogus` isn't declared on the schema flags.
    #[test]
    fn dot_path_schema_field_missing_flagged() {
        let tree = analyze_str(
            r#"{
                #schema U { Int n: * },
                U u: { n: 1 },
                x: u.bogus
            }"#,
        );
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "u.bogus"),
            )
            .collect();
        assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 2.6 reverse: a dot-path through a sibling whose value
    /// comes from a stdlib FnCall (uninferrable type) stays silent —
    /// runtime owns whether the field exists.
    #[test]
    fn dot_path_through_fncall_sibling_silent() {
        let tree = analyze_str(
            r#"{
                xs: range(0, 10),
                first: xs.zero
            }"#,
        );
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name.starts_with("xs.")))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.6 reverse: a dot-path through a sibling whose value is
    /// a typed dict literal with the named key stays silent.
    #[test]
    fn dot_path_existing_key_silent() {
        let tree = analyze_str(r#"{ obj: { a: 1, b: 2 }, x: obj.a }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.5 forward: a spread of a dict literal merges its keys
    /// into the surrounding frame statically — a sibling reference to
    /// one of the spread keys is no longer flagged.
    #[test]
    fn spread_dict_literal_merges_keys_statically() {
        let tree = analyze_str(r#"{ ...{a: 1, b: 2}, x: a + b }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.5 reverse: a spread of a non-literal expression stays
    /// dynamic — references to keys that *might* come from the spread
    /// remain unflagged (the dynamic-spread escape hatch is preserved).
    #[test]
    fn spread_non_literal_still_dynamic() {
        let tree = analyze_str(
            r#"{
                base: { x: 1 },
                merged: { ...&sibling.base, hint: x }
            }"#,
        );
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.4 forward: a closure body's free variable that doesn't
    /// match any in-scope param / sibling and isn't on the stdlib /
    /// host fn allowlist must surface as `UnresolvedReference`.
    #[test]
    fn closure_body_free_var_flagged() {
        let tree = analyze_str(r#"{ helper(x): x + outer_undef }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(
                |d| matches!(d, Diagnostic::UnresolvedReference { name, .. } if name == "outer_undef"),
            )
            .collect();
        assert_eq!(unresolved.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 2.4 reverse: stdlib names like `range` stay silent.
    #[test]
    fn closure_body_stdlib_not_flagged() {
        let tree = analyze_str(r#"{ x: range(0, 10) }"#);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.4 reverse: a host-injected fn name silences the warning.
    #[test]
    fn host_fn_name_silences_unresolved() {
        use std::collections::HashSet;
        let mut host_fn_names = HashSet::new();
        host_fn_names.insert("my_native".to_string());
        let opts = crate::AnalyzeOptions {
            host_fn_names,
            ..Default::default()
        };
        let node = parse_document(r#"{ x: my_native() }"#).unwrap();
        let tree = crate::analyze_with_options(&node, &opts);
        let unresolved: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnresolvedReference { .. }))
            .collect();
        assert!(unresolved.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.2 reverse: a multi-segment slot type (`geo.Location`)
    /// that the per-module pass can't prove unsafe — there's no
    /// in-module `geo` schema name to consult — must stay conservative
    /// (no spurious mismatch). The cross-module form is handled by
    /// the workspace-level `re_check_unknown_types` post-pass.
    #[test]
    fn multi_segment_path_stays_conservative_in_per_module_pass() {
        let tree = analyze_str(
            r#"{
                geo.Location loc: 1,
                #schema X { Int z: * },
                X x_val: { z: 1 }
            }"#,
        );
        // The `loc: 1` slot uses a 2-segment type `geo.Location`. We
        // shouldn't crash and shouldn't push a spurious mismatch in
        // the per-module pass.
        let typ: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "loc"))
            .collect();
        assert!(typ.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.3 forward: a value typed via the derived schema `B` is
    /// accepted in a slot declared `A` when `B` extends `A` through the
    /// `Base + { ... }` composition form.
    #[test]
    fn derived_schema_subsumes_base_slot() {
        let tree = analyze_str(
            r#"{
                #schema A { Int x: * },
                #schema B &sibling.A + { Int y: * },
                B make_b: { x: 1, y: 2 },
                A use_as_a: make_b
            }"#,
        );
        let mm: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "use_as_a"))
            .collect();
        assert!(mm.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 2.3 reverse: an unrelated schema name does not match.
    #[test]
    fn unrelated_schema_does_not_subsume_slot() {
        let tree = analyze_str(
            r#"{
                #schema A { Int x: * },
                #schema C { Int z: * },
                C make_c: { z: 1 },
                A use_as_a: make_c
            }"#,
        );
        // We expect a StaticTypeMismatch because C is not a base / derived
        // of A, but the analyzer's conservative path may also stay silent
        // if it can't prove the negative. The key invariant: if a mismatch
        // does fire, the field should be `use_as_a`.
        for d in &tree.diagnostics {
            if let Diagnostic::StaticTypeMismatch { field, .. } = d {
                assert_eq!(field, "use_as_a");
            }
        }
    }

    // ------------------------------------------------------------------
    // Stage 3 — closure / user fn / stdlib signature lookup + FnCall
    //          arity / arg-type checks. Tests numbered to match the
    //          design doc's §10 coverage list.
    // ------------------------------------------------------------------

    /// Stage 3.7 #1: `range()` with zero args flags `FnCallArgCountMismatch`
    /// (signature requires at least 1 Int).
    #[test]
    fn stage3_range_zero_args_arg_count() {
        let tree = analyze_str(r#"{ x: range() }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgCountMismatch { fn_name, .. } if fn_name == "range"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #2: `range(0, "ten")` — the second arg is a String
    /// where the variadic_tail is Int.
    #[test]
    fn stage3_range_string_arg_arg_type() {
        let tree = analyze_str(r#"{ x: range(0, "ten") }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { fn_name, .. } if fn_name == "range"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #3: `len(123)` — the analyzer signature accepts `Any`
    /// for `len`'s param so this stays silent (v1 doesn't model the
    /// String∣List∣Dict union). Documents the v1 trade-off.
    #[test]
    fn stage3_len_int_silent_v1() {
        let tree = analyze_str(r#"{ x: len(123) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
            .collect();
        assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #4 forward: `len([1,2])` returns Int — `Int n: len(...)`
    /// is happy.
    #[test]
    fn stage3_len_returns_int() {
        let tree = analyze_str(r#"{ Int n: len([1, 2]) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "n"))
            .collect();
        assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #4 reverse: a `String s: len([1,2])` slot mismatches.
    #[test]
    fn stage3_len_returns_int_string_slot_mismatches() {
        let tree = analyze_str(r#"{ String s: len([1, 2]) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "s"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #5: user closure `f(Int x) -> Int: x+1` called with
    /// a String arg flags `FnCallArgTypeMismatch`.
    #[test]
    fn stage3_user_closure_arg_type_mismatch() {
        let tree = analyze_str(r#"{ Int f(Int x): x + 1, y: f("str") }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { fn_name, .. } if fn_name == "f"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #6: user closure return type drives slot inference —
    /// `String y: f(1)` mismatches because `f` returns Int.
    #[test]
    fn stage3_user_closure_return_drives_slot() {
        let tree = analyze_str(r#"{ Int f(Int x): x + 1, String y: f(1) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "y"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #7: `_math_abs()` — zero args.
    #[test]
    fn stage3_math_abs_no_args() {
        let tree = analyze_str(r#"{ x: _math_abs() }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgCountMismatch { fn_name, .. } if fn_name == "_math_abs"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #8: `_string_upper(123)` — wrong type.
    #[test]
    fn stage3_string_upper_int_arg() {
        let tree = analyze_str(r#"{ x: _string_upper(123) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { fn_name, .. } if fn_name == "_string_upper"))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #9 reverse: `range(0, 10)` is legal (uses variadic_tail).
    #[test]
    fn stage3_range_two_args_legal() {
        let tree = analyze_str(r#"{ x: range(0, 10) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::FnCallArgCountMismatch { .. }
                        | Diagnostic::FnCallArgTypeMismatch { .. }
                )
            })
            .collect();
        assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #10 reverse: undefined fn name silently falls through
    /// the FnCall checker (still emits `UnresolvedReference`, but no
    /// FnCall diagnostic).
    #[test]
    fn stage3_undefined_fn_silent_on_signature_check() {
        let tree = analyze_str(r#"{ f(): undefined() }"#);
        let fn_call_diags: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::FnCallArgCountMismatch { .. }
                        | Diagnostic::FnCallArgTypeMismatch { .. }
                )
            })
            .collect();
        assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #11 reverse: a host fn registered without a signature
    /// silently passes the FnCall check.
    #[test]
    fn stage3_host_fn_without_sig_silent() {
        use std::collections::HashSet;
        let mut host_fn_names = HashSet::new();
        host_fn_names.insert("my_native".to_string());
        let opts = crate::AnalyzeOptions {
            host_fn_names,
            ..Default::default()
        };
        let node = parse_document(r#"{ x: my_native(1, 2, 3) }"#).unwrap();
        let tree = crate::analyze_with_options(&node, &opts);
        let fn_call_diags: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::FnCallArgCountMismatch { .. }
                        | Diagnostic::FnCallArgTypeMismatch { .. }
                )
            })
            .collect();
        assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #12 reverse: an arg whose type is dynamic (`Any`)
    /// silently passes the per-arg check.
    #[test]
    fn stage3_dynamic_arg_silent() {
        // The `_string_upper` param is String. Pass an unresolvable
        // identifier (silent on inference) → arg infer returns None →
        // the per-arg check `continue`s.
        let tree = analyze_str(r#"{ f(x): _string_upper(x) }"#);
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
            .collect();
        assert!(mismatches.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #13 (consistency): when the analyzer reports an
    /// `FnCallArgTypeMismatch`, `tree.has_errors()` is true so the
    /// evaluator's facade refuses to run before reaching the runtime
    /// path. Pins the contract that analyzer-reported errors gate
    /// evaluation.
    #[test]
    fn stage3_arg_type_mismatch_marks_tree_errored() {
        let tree = analyze_str(r#"{ Int f(Int x): x + 1, y: f("str") }"#);
        assert!(tree.has_errors(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #14 (known short-fall): cross-module fn imports v1
    /// silent. The signature lookup only sees stdlib + host + same-
    /// file closures; an `#import`ed user closure has no signature
    /// reachable here, so the call goes unchecked. This test pins the
    /// v1 limitation explicitly so a later v1.1 stage can flip it.
    #[test]
    fn stage3_cross_module_fn_import_silent_v1() {
        // A bare-`Variable("module_alias")` head doesn't even reach
        // FnCall handling — but a `module.fn(arg)` form does. We verify
        // it stays silent against the FnCall checker. Modeling cross-
        // module signatures is deferred to v1.1.
        let tree = analyze_str(r#"{ x: imported.fn(1, 2, 3) }"#);
        let fn_call_diags: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::FnCallArgCountMismatch { .. }
                        | Diagnostic::FnCallArgTypeMismatch { .. }
                )
            })
            .collect();
        assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #15: dict-literal sibling closure call (`utils.greet`)
    /// resolves to the closure's signature and accepts a legal arg.
    #[test]
    fn stage3_sibling_closure_dict_literal_call_silent() {
        let tree = analyze_str(
            r#"{
                utils: { greet(s): "hi" + s },
                x: utils.greet("a")
            }"#,
        );
        let fn_call_diags: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Diagnostic::FnCallArgCountMismatch { .. }
                        | Diagnostic::FnCallArgTypeMismatch { .. }
                )
            })
            .collect();
        assert!(fn_call_diags.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 3.7 #16: same dict-literal sibling form, but the closure
    /// declares `String s` and the call passes an Int — the analyzer
    /// flags `FnCallArgTypeMismatch`.
    #[test]
    fn stage3_sibling_closure_dict_literal_arg_type_mismatch() {
        let tree = analyze_str(
            r#"{
                utils: { greet(String s): "hi" + s },
                x: utils.greet(123)
            }"#,
        );
        let mismatches: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
            .collect();
        assert_eq!(mismatches.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 3.2 drift defense: every name registered by the
    /// evaluator's `stdlib::register_to` must have a signature in
    /// `stdlib_signatures`. If a maintainer adds a new fn to the
    /// evaluator without updating the analyzer table, this test
    /// fails — keeping the two views in lockstep.
    #[test]
    fn stage3_stdlib_signatures_cover_all_register_fn_names() {
        let sigs = crate::stdlib_signatures::stdlib_signatures();
        let names = stdlib_registered_names();
        let missing: Vec<&&str> = names.iter().filter(|n| !sigs.contains_key(**n)).collect();
        assert!(
            missing.is_empty(),
            "stdlib functions without analyzer signatures: {missing:?}"
        );
    }

    // ----- Stage 5: const-folding diagnostics -----------------------

    fn const_div_zero_count(tree: &AnalyzedTree) -> usize {
        tree.diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::ConstDivisionByZero { .. }))
            .count()
    }

    fn const_overflow_count(tree: &AnalyzedTree) -> usize {
        tree.diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::ConstNumericOverflow { .. }))
            .count()
    }

    #[test]
    fn stage5_div_by_zero_literal() {
        let tree = analyze_str(r#"{ x: 1 / 0 }"#);
        assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
        assert!(tree.has_errors());
    }

    #[test]
    fn stage5_mod_by_zero_literal() {
        let tree = analyze_str(r#"{ x: 100 % 0 }"#);
        assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
    }

    #[test]
    fn stage5_overflow_add_at_max_plus_one() {
        let tree = analyze_str(r#"{ x: 9223372036854775807 + 1 }"#);
        assert_eq!(const_overflow_count(&tree), 1, "{:?}", tree.diagnostics);
        assert!(tree.has_errors());
    }

    #[test]
    fn stage5_overflow_chained_mul() {
        // 1_000_000^4 = 1e24 > i64::MAX, traps on the third multiply.
        let tree = analyze_str(r#"{ x: 1000000 * 1000000 * 1000000 * 1000000 }"#);
        assert_eq!(const_overflow_count(&tree), 1, "{:?}", tree.diagnostics);
    }

    #[test]
    fn stage5_subtree_folds_then_div_zero() {
        // (1+2)*(3+4)/0 → fold collapses to 21/0, single diagnostic.
        let tree = analyze_str(r#"{ x: (1 + 2) * (3 + 4) / 0 }"#);
        assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
        // No overflow false-positive on the inner sub-expressions.
        assert_eq!(const_overflow_count(&tree), 0);
    }

    #[test]
    fn stage5_unary_neg_i64_min_overflows() {
        // i64::MIN = -9223372036854775808 — the `-` unary on
        // `9223372036854775807 + 1` would itself overflow first; we use
        // the canonical hex form via `(-9223372036854775807 - 1)` to
        // construct i64::MIN as an Int and then unary-negate it.
        let tree = analyze_str(r#"{ x: -(-9223372036854775807 - 1) }"#);
        assert_eq!(const_overflow_count(&tree), 1, "{:?}", tree.diagnostics);
    }

    #[test]
    fn stage5_variable_in_subtree_silent() {
        // `a + 1` references a sibling, so the fold pass returns None
        // and runtime keeps the verdict.
        let tree = analyze_str(r#"{ a: 1, x: a + 1 }"#);
        assert_eq!(const_div_zero_count(&tree), 0);
        assert_eq!(const_overflow_count(&tree), 0);
    }

    #[test]
    fn stage5_float_div_zero_silent() {
        // 1.0 / 0.0 is +Inf in IEEE-754 — never errors.
        let tree = analyze_str(r#"{ x: 1.0 / 0.0 }"#);
        assert_eq!(const_div_zero_count(&tree), 0, "{:?}", tree.diagnostics);
        assert_eq!(const_overflow_count(&tree), 0);
    }

    #[test]
    fn stage5_fn_call_in_subtree_silent() {
        // `len([1,2,3])` is non-foldable (FnCall); whole expression
        // defers to runtime.
        let tree = analyze_str(r#"{ x: 1 / len([1, 2, 3]) }"#);
        assert_eq!(const_div_zero_count(&tree), 0, "{:?}", tree.diagnostics);
    }

    #[test]
    fn stage5_ternary_node_itself_does_not_fold() {
        // The Ternary expression is *not* foldable as a whole — even if
        // both branches look literal, branch selection is data-driven.
        // BUT the walker still descends into each branch, and a `1 / 0`
        // Binary inside the `then` arm is a real sub-node that the
        // walker hands to `check_const_fold`. Mirroring the List case
        // below, we *do* expect the inner literal to fire.
        let tree = analyze_str(r#"{ cond: true, x: cond ? 1 / 0 : 0 }"#);
        assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
    }

    #[test]
    fn stage5_ternary_with_runtime_in_branch_silent() {
        // Variant where the branch contains a non-literal — the inner
        // `a / 0` walker visit still folds, but the divisor is a literal
        // 0 so it *should* still fire. To test the "no-fire when
        // operand isn't literal" path we put the data-dependence on the
        // dividend side and divide by a non-zero constant: `cond ? a /
        // 1 : 0` should be silent because the inner Binary's left is a
        // Variable head (no fold) and the divisor isn't zero.
        let tree = analyze_str(r#"{ a: 1, cond: true, x: cond ? a / 1 : 0 }"#);
        assert_eq!(const_div_zero_count(&tree), 0, "{:?}", tree.diagnostics);
        assert_eq!(const_overflow_count(&tree), 0);
    }

    #[test]
    fn stage5_div_zero_inside_list_still_fires() {
        // The list itself isn't foldable but the walker descends into
        // every list element — the inner `1 / 0` is still a Binary
        // node visited by the walker, so the diagnostic still fires.
        let tree = analyze_str(r#"{ x: [1 / 0] }"#);
        assert_eq!(const_div_zero_count(&tree), 1, "{:?}", tree.diagnostics);
    }

    #[test]
    fn stage5_diagnostic_blocks_evaluation_via_has_errors() {
        // Stage 5 promotes ConstDivisionByZero / ConstNumericOverflow
        // to Severity::Error so `has_errors()` returns true and hosts
        // following the documented "skip eval on errors" pattern keep
        // the runtime out.
        let tree = analyze_str(r#"{ x: 9223372036854775807 + 1 }"#);
        assert!(tree.has_errors(), "{:?}", tree.diagnostics);
        assert_eq!(
            tree.diagnostics
                .iter()
                .find(|d| matches!(d, Diagnostic::ConstNumericOverflow { .. }))
                .map(|d| d.severity()),
            Some(crate::Severity::Error)
        );
    }

    // ----- v1.1: generic instantiation (List<T> / Result<T,E> -----

    fn fn_call_arg_mismatch_count(tree: &AnalyzedTree) -> usize {
        tree.diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::FnCallArgTypeMismatch { .. }))
            .count()
    }

    fn static_mismatch_count(tree: &AnalyzedTree) -> usize {
        tree.diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::StaticTypeMismatch { .. }))
            .count()
    }

    /// `_list_map(["a","b"], (s) => s)` returns `List<String>`; placing
    /// it in a `List<Int>` slot must flag a static mismatch derivable
    /// purely from source + stdlib signatures.
    #[test]
    fn v1_1_list_map_return_type_mismatches_int_slot() {
        let tree = analyze_str(r#"{ List<Int> xs: _list_map(["a", "b"], (s) => s) }"#);
        assert!(static_mismatch_count(&tree) >= 1, "{:?}", tree.diagnostics);
        assert!(tree.has_errors());
    }

    /// Inverse: `List<String>` slot, mapping `[1,2,3]` through
    /// `(n) => n` returns `List<Int>` — should also flag.
    #[test]
    fn v1_1_list_map_return_type_mismatches_string_slot() {
        let tree = analyze_str(r#"{ List<String> xs: _list_map([1, 2, 3], (n) => n) }"#);
        assert!(static_mismatch_count(&tree) >= 1, "{:?}", tree.diagnostics);
        assert!(tree.has_errors());
    }

    /// `_list_contains([1,2], "x")` — `T` binds `Int` from arg 0; the
    /// String literal in arg 1 then mismatches the substituted `T`
    /// slot.
    #[test]
    fn v1_1_list_contains_arg_type_mismatch_after_unification() {
        let tree = analyze_str(r#"{ Bool b: _list_contains([1, 2], "x") }"#);
        assert!(
            fn_call_arg_mismatch_count(&tree) >= 1,
            "{:?}",
            tree.diagnostics
        );
        assert!(tree.has_errors());
    }

    /// Negative: `List<Int> xs: _list_map([1,2,3], (n) => n + 1)` —
    /// `T → Int`, body type `Int`, `U → Int`, return `List<Int>`,
    /// matches the slot. Should produce zero static / FnCall arg
    /// mismatches related to the call.
    #[test]
    fn v1_1_list_map_int_to_int_passes() {
        let tree = analyze_str(r#"{ List<Int> xs: _list_map([1, 2, 3], (n) => n + 1) }"#);
        let irrelevant_ok = tree.diagnostics.iter().all(|d| {
            !matches!(
                d,
                Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        });
        assert!(irrelevant_ok, "{:?}", tree.diagnostics);
    }

    /// Negative: `_list_contains` with a same-typed needle stays
    /// silent.
    #[test]
    fn v1_1_list_contains_same_type_passes() {
        let tree = analyze_str(r#"{ Bool b: _list_contains([1, 2, 3], 2) }"#);
        let irrelevant_ok = tree.diagnostics.iter().all(|d| {
            !matches!(
                d,
                Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        });
        assert!(irrelevant_ok, "{:?}", tree.diagnostics);
    }

    /// Negative: `_list_reduce([1,2,3], 0, (acc, x) => acc + x)` —
    /// `T → Int` (from arg 0), `U → Int` (from `init`), body type
    /// `Int`. Return slot `U` reads as `Int`, matches the
    /// `Int s:` binding.
    #[test]
    fn v1_1_list_reduce_int_init_passes_int_slot() {
        let tree = analyze_str(r#"{ Int s: _list_reduce([1, 2, 3], 0, (acc, x) => acc + x) }"#);
        let irrelevant_ok = tree.diagnostics.iter().all(|d| {
            !matches!(
                d,
                Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        });
        assert!(irrelevant_ok, "{:?}", tree.diagnostics);
    }

    /// Consistency: when the analyzer reports a v1.1 mismatch, the
    /// tree must have `has_errors() == true` so hosts that follow
    /// the documented "skip eval on errors" pattern won't reach
    /// the evaluator's runtime path. (The v1.1 report flips the
    /// "is this caught statically?" answer to yes.)
    #[test]
    fn v1_1_static_mismatch_marks_tree_as_errored() {
        let tree = analyze_str(r#"{ Bool b: _list_contains([1, 2], "x") }"#);
        assert!(tree.has_errors(), "{:?}", tree.diagnostics);
    }

    // ============= v1.3 strict-mode tests =============

    fn count(tree: &AnalyzedTree, pred: impl Fn(&Diagnostic) -> bool) -> usize {
        tree.diagnostics.iter().filter(|d| pred(d)).count()
    }

    /// v1.3 forward: in strict mode, an untyped non-dict spread
    /// (`...e` where `e` isn't a dict literal) reports
    /// `SpreadSourceTypeUnknown`.
    #[test]
    fn v1_3_strict_spread_without_type_flagged() {
        let tree = analyze_str(
            r#"
            { src: 1 + 2, ...src }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// Reverse: under `#relaxed`, the same spread is silent.
    #[test]
    fn relaxed_spread_silent() {
        let tree = analyze_str(
            r#"#relaxed
            { src: 1 + 2, ...src }"#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.3 reverse: typed spread `...<Extra> e` is silent under
    /// strict mode (the user provided the hint).
    #[test]
    fn v1_3_strict_typed_spread_silent() {
        let tree = analyze_str(
            r#"
            #schema Extra { Int a: *, Int b: * }
            { src: { a: 1, b: 2 }, ...<Extra> src }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.3 forward: dynamic key without typehint flagged in strict
    /// mode.
    #[test]
    fn v1_3_strict_dynamic_key_without_type_flagged() {
        let tree = analyze_str(
            r#"
            { k: "key", [k]: 1 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.3 reverse: typed dynamic key `[<String> k]:` silent.
    #[test]
    fn v1_3_strict_typed_dynamic_key_silent() {
        let tree = analyze_str(
            r#"
            { k: "key", [<String> k]: 1 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// Reverse: under `#relaxed`, an untyped dynamic key is silent.
    #[test]
    fn relaxed_dynamic_key_silent() {
        let tree = analyze_str(
            r#"#relaxed
            { k: "key", [k]: 1 }"#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.3 forward: DuplicateField fires (both modes) when a spread
    /// of a known-shape source contributes a key that's already
    /// declared.
    #[test]
    fn v1_3_duplicate_field_named_vs_typed_spread() {
        let tree = analyze_str(
            r#"
            #schema Extra { Int a: *, Int b: * }
            { src: { a: 1, b: 2 }, a: 99, ...<Extra> src }
            "#,
        );
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::DuplicateField { field, .. } if field == "a"),
        );
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.3 forward: DuplicateField fires across two spreads of dict
    /// literals that overlap.
    #[test]
    fn v1_3_duplicate_field_two_spreads_overlap() {
        let tree = analyze_str(r#"{ ...{ a: 1 }, ...{ a: 2 } }"#);
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::DuplicateField { field, .. } if field == "a"),
        );
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.3 reverse: DuplicateField does not fire when the spread's
    /// keys are unknown (untyped non-literal source) — we only emit
    /// when we can statically prove the conflict.
    #[test]
    fn v1_3_duplicate_field_silent_when_spread_dynamic() {
        let tree = analyze_str(
            r#"
            { src: outer_value, a: 99, ...src }
            "#,
        );
        let n = count(&tree, |d| matches!(d, Diagnostic::DuplicateField { .. }));
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.3 forward: strict mode demands UnresolvedSchema for a typed
    /// spread whose schema isn't declared.
    #[test]
    fn v1_3_strict_spread_unresolved_schema_flagged() {
        let tree = analyze_str(
            r#"
            { src: 1, ...<Mystery> src }
            "#,
        );
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::UnresolvedSchema { name, .. } if name == "Mystery"),
        );
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.3 reverse: when the schema *is* declared, no
    /// UnresolvedSchema fires.
    #[test]
    fn v1_3_strict_spread_known_schema_silent() {
        let tree = analyze_str(
            r#"
            #schema Extra { Int a: * }
            { src: { a: 1 }, ...<Extra> src }
            "#,
        );
        let n = count(&tree, |d| matches!(d, Diagnostic::UnresolvedSchema { .. }));
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// AnalyzedTree carries `strict_mode = true` by default — no
    /// directive needed.
    #[test]
    fn strict_mode_bit_set_by_default() {
        let tree = analyze_str("{ a: 1 }");
        assert!(tree.strict_mode);
    }

    /// `#relaxed` clears the strict_mode bit.
    #[test]
    fn relaxed_directive_clears_strict_mode_bit() {
        let tree = analyze_str(
            r#"#relaxed
            { a: 1 }"#,
        );
        assert!(!tree.strict_mode);
    }

    /// `#unstrict` is a synonym for `#relaxed`; it also clears the bit.
    #[test]
    fn unstrict_directive_clears_strict_mode_bit() {
        let tree = analyze_str(
            r#"#unstrict
            { a: 1 }"#,
        );
        assert!(!tree.strict_mode);
    }

    /// Strict (the default) + native fn without static signature
    /// should report `NativeFnSignatureMissing`. We simulate via an
    /// `AnalyzeOptions::host_fn_names` entry without a corresponding
    /// signature.
    #[test]
    fn strict_native_fn_signature_missing_without_signature() {
        let src = "{ x: my_native(1, 2) }";
        let node = parse_document(src).unwrap();
        let mut names = std::collections::HashSet::new();
        names.insert("my_native".to_string());
        let opts = crate::AnalyzeOptions {
            host_fn_names: names,
            host_fn_signatures: HashMap::new(),
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: true,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::NativeFnSignatureMissing { fn_name, .. } if fn_name == "my_native"),
        );
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.3 reverse: same shape, but non-strict — silent.
    #[test]
    fn v1_3_non_strict_native_call_silent() {
        let src = "{ x: my_native(1, 2) }";
        let node = parse_document(src).unwrap();
        let mut names = std::collections::HashSet::new();
        names.insert("my_native".to_string());
        let opts = crate::AnalyzeOptions {
            host_fn_names: names,
            host_fn_signatures: HashMap::new(),
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: false,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::NativeFnSignatureMissing { .. })
        });
        assert_eq!(n, 0);
    }

    /// v1.3: strict + native fn *with* a signature — silent.
    #[test]
    fn v1_3_strict_native_with_signature_silent() {
        let src = "{ x: my_native(1, 2) }";
        let node = parse_document(src).unwrap();
        let mut names = std::collections::HashSet::new();
        names.insert("my_native".to_string());
        let mut sigs: HashMap<String, FnSignature> = HashMap::new();
        sigs.insert(
            "my_native".to_string(),
            FnSignature {
                name: "my_native".to_string(),
                generics: Vec::new(),
                params: vec![
                    FnParam {
                        name: "a".to_string(),
                        ty: type_node_simple("Int"),
                        optional: false,
                    },
                    FnParam {
                        name: "b".to_string(),
                        ty: type_node_simple("Int"),
                        optional: false,
                    },
                ],
                return_type: type_node_simple("Int"),
                variadic_tail: None,
            },
        );
        let opts = crate::AnalyzeOptions {
            host_fn_names: names,
            host_fn_signatures: sigs,
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: false,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::NativeFnSignatureMissing { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.8 (C4): host fn signature with `Any` parameter raises
    /// `ExplicitAnyForbidden` carrying a `host fn` context.
    #[test]
    fn v1_8_host_fn_signature_any_param_rejected() {
        let node = relon_parser::parse_document("{ x: 1 }").unwrap();
        let mut sigs = HashMap::new();
        sigs.insert(
            "my_native".to_string(),
            crate::FnSignature {
                name: "my_native".to_string(),
                generics: Vec::new(),
                params: vec![FnParam {
                    name: "blob".to_string(),
                    ty: type_node_simple("Any"),
                    optional: false,
                }],
                return_type: type_node_simple("Int"),
                variadic_tail: None,
            },
        );
        let opts = crate::AnalyzeOptions {
            host_fn_names: HashSet::new(),
            host_fn_signatures: sigs,
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: false,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(&tree, |d| {
            matches!(
                d,
                Diagnostic::ExplicitAnyForbidden { context, .. }
                    if context.contains("host fn 'my_native'")
                        && context.contains("'blob'")
            )
        });
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.8 (C4): host fn return-type `Any` raises
    /// `ExplicitAnyForbidden` with a return-type context label.
    #[test]
    fn v1_8_host_fn_signature_any_return_rejected() {
        let node = relon_parser::parse_document("{ x: 1 }").unwrap();
        let mut sigs = HashMap::new();
        sigs.insert(
            "fetch".to_string(),
            crate::FnSignature {
                name: "fetch".to_string(),
                generics: Vec::new(),
                params: vec![FnParam {
                    name: "url".to_string(),
                    ty: type_node_simple("String"),
                    optional: false,
                }],
                return_type: type_node_simple("Any"),
                variadic_tail: None,
            },
        );
        let opts = crate::AnalyzeOptions {
            host_fn_names: HashSet::new(),
            host_fn_signatures: sigs,
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: false,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(&tree, |d| {
            matches!(
                d,
                Diagnostic::ExplicitAnyForbidden { context, .. }
                    if context.contains("host fn 'fetch'")
                        && context.contains("return type")
            )
        });
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.8 (C4): host fn signature with bare `List` param raises
    /// `BareGenericContainer`.
    #[test]
    fn v1_8_host_fn_signature_bare_list_rejected() {
        let node = relon_parser::parse_document("{ x: 1 }").unwrap();
        let mut sigs = HashMap::new();
        sigs.insert(
            "len_of".to_string(),
            crate::FnSignature {
                name: "len_of".to_string(),
                generics: Vec::new(),
                params: vec![FnParam {
                    name: "xs".to_string(),
                    ty: type_node_simple("List"),
                    optional: false,
                }],
                return_type: type_node_simple("Int"),
                variadic_tail: None,
            },
        );
        let opts = crate::AnalyzeOptions {
            host_fn_names: HashSet::new(),
            host_fn_signatures: sigs,
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: false,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(&tree, |d| {
            matches!(
                d,
                Diagnostic::BareGenericContainer { type_name, context, .. }
                    if type_name == "List" && context.contains("host fn 'len_of'")
            )
        });
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.8 (C4): variadic-tail `Any` is also flagged.
    #[test]
    fn v1_8_host_fn_signature_any_variadic_rejected() {
        let node = relon_parser::parse_document("{ x: 1 }").unwrap();
        let mut sigs = HashMap::new();
        sigs.insert(
            "log".to_string(),
            crate::FnSignature {
                name: "log".to_string(),
                generics: Vec::new(),
                params: Vec::new(),
                return_type: type_node_simple("Null"),
                variadic_tail: Some(type_node_simple("Any")),
            },
        );
        let opts = crate::AnalyzeOptions {
            host_fn_names: HashSet::new(),
            host_fn_signatures: sigs,
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: false,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(&tree, |d| {
            matches!(
                d,
                Diagnostic::ExplicitAnyForbidden { context, .. }
                    if context.contains("host fn 'log'") && context.contains("variadic tail")
            )
        });
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.8 (C4): a clean signature using concrete types and unbound
    /// generics raises no host-fn ban-Any / ban-bare diagnostics.
    #[test]
    fn v1_8_host_fn_signature_clean_silent() {
        let node = relon_parser::parse_document("{ x: 1 }").unwrap();
        let mut sigs = HashMap::new();
        sigs.insert(
            "id".to_string(),
            crate::FnSignature {
                name: "id".to_string(),
                generics: vec!["T".to_string()],
                params: vec![FnParam {
                    name: "v".to_string(),
                    ty: type_node_simple("T"),
                    optional: false,
                }],
                return_type: type_node_simple("T"),
                variadic_tail: None,
            },
        );
        let opts = crate::AnalyzeOptions {
            host_fn_names: HashSet::new(),
            host_fn_signatures: sigs,
            host_fn_gates: HashMap::new(),
            caps: crate::Capabilities::default(),
            strict_mode: false,
        };
        let tree = crate::analyze_with_options(&node, &opts);
        let n = count(&tree, |d| {
            matches!(
                d,
                Diagnostic::ExplicitAnyForbidden { context, .. }
                    | Diagnostic::BareGenericContainer { context, .. }
                    if context.contains("host fn")
            )
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.3 boundary: typed dynamic key with Int — silent.
    #[test]
    fn v1_3_typed_int_dynkey_silent() {
        let tree = analyze_str(
            r#"
            { idx: 0, [<Int> idx]: "row0" }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.3 boundary: typed dynamic key whose expression is a binary
    /// op — silent.
    #[test]
    fn v1_3_typed_expr_dynkey_silent() {
        let tree = analyze_str(
            r#"
            { a: "x", b: "y", [<String> a + b]: 1 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::DynamicKeyTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    // ============= v1.4 strict completeness tests =============

    /// v1.4 forward: under strict mode, a path tail descending into a
    /// schema's missing field reports `UnknownReferenceType` with the
    /// failing segment as `name` and the full path as the `path`
    /// vector.
    #[test]
    fn v1_4_strict_path_tail_unknown_field() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, Float total: * }
            #main(Order o) -> Dict
            { x: o.unknown }
            "#,
        );
        let hits: Vec<_> = tree
            .diagnostics
            .iter()
            .filter_map(|d| match d {
                Diagnostic::UnknownReferenceType { name, path, .. } => {
                    Some((name.clone(), path.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(hits.len(), 1, "{:?}", tree.diagnostics);
        assert_eq!(hits[0].0, "unknown");
        assert_eq!(hits[0].1, vec!["o".to_string(), "unknown".to_string()]);
    }

    /// v1.4 forward: descending into a leaf type (`Int` has no fields)
    /// produces `UnknownReferenceType` on the failing segment.
    #[test]
    fn v1_4_strict_path_tail_int_descend() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, Float total: * }
            #main(Order o) -> Dict
            { x: o.id.something }
            "#,
        );
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "something"),
        );
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.4 reverse: a fully classified path (`o.id` → Int) under
    /// strict mode silently passes — no UnknownReferenceType.
    #[test]
    fn v1_4_strict_path_tail_known_field_silent() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, Float total: * }
            #main(Order o) -> Dict
            { x: o.id }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::UnknownReferenceType { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// Even under `#relaxed`, the path-tail walker reports
    /// `UnknownReferenceType` for a positively-known broken step
    /// (`o.unknown` on `Order` with no such field). The analyzer has
    /// the schema's field index, so the failure is a static error
    /// regardless of mode.
    #[test]
    fn non_strict_path_tail_reports_unknown_ref_type() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, Float total: * }
            #main(Order o) -> Dict<String, Int>
            { x: o.unknown }
            "#,
        );
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "unknown"),
        );
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: strict mode reports `UnknownReferenceType`
    /// against a multi-hop chain whose final step lands on a leaf
    /// (`o.customer.name.upper`).
    #[test]
    fn v1_4_strict_multi_hop_string_leaf_descend() {
        let tree = analyze_str(
            r#"
            #schema Customer { String name: * }
            #schema Order { Customer customer: *, Int id: * }
            #main(Order o) -> Dict
            { x: o.customer.name.upper }
            "#,
        );
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "upper"),
        );
        assert_eq!(n, 1, "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: strict mode + path-spread of a typed schema field
    /// (`...o.extras` where `Order.extras : Extras`). The v1.3 walker
    /// would have demanded an explicit `<T>` typehint; v1.4 derives it
    /// from the path-tail walker.
    #[test]
    fn v1_4_strict_path_spread_schema_silent() {
        let tree = analyze_str(
            r#"
            #schema Extras { Int a: *, Int b: * }
            #schema Order { Extras extras: *, Int id: * }
            #main(Order o) -> Dict
            { id: o.id, ...o.extras }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: strict mode + path-spread of a `Dict<K,V>` field
    /// — the value type is fully classified even though keys are
    /// dynamic. No SpreadSourceTypeUnknown.
    #[test]
    fn v1_4_strict_path_spread_dict_silent() {
        let tree = analyze_str(
            r#"
            #schema Order { Dict<String, Int> kv: *, Int id: * }
            #main(Order o) -> Dict
            { id: o.id, ...o.kv }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: strict mode + FnCall-spread (`...load_extras()`)
    /// where `load_extras` is a sibling closure declared with `->
    /// Extras`. The signature is harvested by the v1.4 pre-pass before
    /// the spread check runs.
    #[test]
    fn v1_4_strict_fncall_spread_schema_silent() {
        let tree = analyze_str(
            r#"
            #schema Extras { Int a: *, Int b: * }
            {
              Extras src: { a: 1, b: 2 },
              load_extras: () -> Extras => src,
              ...load_extras()
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: strict mode + path-spread whose tail-walk fails
    /// (`...o.unknown`). Strict reports the more specific
    /// UnknownReferenceType so the user sees the precise step that
    /// stalled.
    #[test]
    fn v1_4_strict_path_spread_unknown_reports_specific() {
        let tree = analyze_str(
            r#"
            #schema Extras { Int a: *, Int b: * }
            #schema Order { Extras extras: *, Int id: * }
            #main(Order o) -> Dict
            { id: o.id, ...o.unknown }
            "#,
        );
        let unk = count(
            &tree,
            |d| matches!(d, Diagnostic::UnknownReferenceType { name, .. } if name == "unknown"),
        );
        assert!(unk >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: strict mode + a typed binding whose value is a
    /// well-formed list comprehension. The v1.5 inference engine now
    /// derives `List<Int>` for `[x * 2 for x in range(5) if x > 0]`,
    /// matching the declared `List<Int>` slot — no diagnostic.
    /// (Pre-v1.5 this was an `ExpressionTypeUnknown` because the comprehension
    /// was unconditionally opaque.)
    #[test]
    fn v1_5_strict_typed_binding_comprehension_inferable() {
        let tree = analyze_str(
            r#"
            { List<Int> xs: [x * 2 for x in range(5) if x > 0] }
            "#,
        );
        // No ExpressionTypeUnknown / StaticTypeMismatch — strict mode is
        // satisfied by the comprehension's derived element type.
        let il = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
        });
        let stm = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(il, 0, "{:?}", tree.diagnostics);
        assert_eq!(stm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.4 reverse: a typed binding whose value *is* inferrable
    /// (literal Int) doesn't fire ExpressionTypeUnknown even under strict
    /// mode.
    #[test]
    fn v1_4_strict_typed_binding_inferrable_silent() {
        let tree = analyze_str(
            r#"
            { Int x: 42 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.4 forward: strict mode + a match arm whose body relies on
    /// an unknown call — ExpressionTypeUnknown pinned on the arm body.
    #[test]
    fn v1_4_strict_match_arm_uninferrable() {
        let tree = analyze_str(
            r#"
            #schema Status Enum<"on", "off">
            #main(Status s) -> Dict
            { result: s match { on: mystery_call(), off: 0 } }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
                if reason.contains("match arm"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.4 reverse: strict mode + a match where every arm is
    /// inferrable — no ExpressionTypeUnknown. Verifies the strict-aware
    /// walker doesn't false-flag well-typed matches.
    #[test]
    fn v1_4_strict_match_arms_inferrable_silent() {
        let tree = analyze_str(
            r#"
            #schema Status Enum<"on", "off">
            #main(Status s) -> Dict
            { result: s match { on: 1, off: 0 } }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.8+ regression: an inline `<Dict<String, Int>>` typehint on a
    /// dynamic spread is the documented strict-mode escape hatch
    /// (`docs/zh/guide/spec.md` §6.6). Pre-fix the `spread_source_schema`
    /// helper returned `Some("Dict")` because it took `path[0]` blindly,
    /// then `schema_known("Dict")` was `false`, so the analyzer pushed
    /// a bogus `UnresolvedSchema("Dict")`. The fix skips builtin heads
    /// before treating them as schema names; `spread_source_is_dict`
    /// owns the Dict-typed-spread classification path.
    #[test]
    fn v1_8e_strict_dict_typehint_spread_silent() {
        let tree = analyze_str(
            r#"
            #main(Dict<String, Int> kv) -> Dict
            {
              base: 1,
              ...<Dict<String, Int>> kv
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(
                d,
                Diagnostic::UnresolvedSchema { name, .. } if name == "Dict"
            )
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
        let m = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(m, 0, "{:?}", tree.diagnostics);
    }

    /// v1.4 boundary: spread source resolves to `Dict<String, Int>`
    /// via FnCall return — strict accepts. Pairs with the
    /// `path_spread_dict` fixture for the path side.
    #[test]
    fn v1_4_strict_fncall_spread_dict_silent() {
        let tree = analyze_str(
            r#"
            {
              Dict<String, Int> seed: { x: 1 },
              load_kv: () -> Dict<String, Int> => seed,
              ...load_kv()
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    // ============= v1.5 strict completeness — kill the long tail =============

    /// v1.5 forward: list comprehension under a typed `List<Int>` slot
    /// produces the matching element type and silences strict checks
    /// that previously fired `ExpressionTypeUnknown`.
    #[test]
    fn v1_5_strict_comprehension_list_int_silent() {
        let tree = analyze_str(
            r#"
            { List<Int> doubled: [x * 2 for x in range(5)] }
            "#,
        );
        let il = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
        });
        let stm = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(il, 0, "{:?}", tree.diagnostics);
        assert_eq!(stm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: comprehension binding's element type now flows
    /// into the body's expression scope, so `x * 2` infers `Int` and
    /// the resulting `List<Int>` mismatches a `List<String>` slot
    /// statically.
    #[test]
    fn v1_5_strict_comprehension_element_mismatch() {
        let tree = analyze_str(
            r#"
            { List<String> xs: [x * 2 for x in range(5)] }
            "#,
        );
        let stm = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert!(stm >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: where-expression's body is inferred under a scope
    /// extended with the bindings — `(n + 1)` infers Int when `n` was
    /// bound to `x: Int`.
    #[test]
    fn v1_5_strict_where_body_int_silent() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> Int
            (n + 1) where { n: x }
            "#,
        );
        let mm = count(&tree, |d| {
            matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
        });
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: where-body Int leaks against String return.
    #[test]
    fn v1_5_strict_where_body_string_mismatch() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> String
            (n + 1) where { n: x }
            "#,
        );
        let mm = count(&tree, |d| {
            matches!(d, Diagnostic::MainReturnTypeMismatch { expected, found, .. }
                if expected == "String" && found == "Int")
        });
        assert_eq!(mm, 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: untyped closure parameter under strict mode.
    #[test]
    fn v1_5_strict_closure_untyped_param_flagged() {
        let tree = analyze_str(
            r#"
            { f: (n) => n + 1 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ClosureParamTypeMissing { param_name, .. }
                if param_name == "n")
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 reverse: typed closure parameter is silent.
    #[test]
    fn v1_5_strict_closure_typed_param_silent() {
        let tree = analyze_str(
            r#"
            { f: (Int n) -> Int => n + 1 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ClosureParamTypeMissing { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: typed param but body relies on an unknown call,
    /// no declared `-> ReturnType`. Body inference yields `Any` →
    /// ClosureReturnTypeUnknown.
    #[test]
    fn v1_5_strict_closure_unclassified_body_flagged() {
        let tree = analyze_str(
            r#"
            { f: (Int n) => mystery(n) }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ClosureReturnTypeUnknown { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 reverse: declared `-> ReturnType` makes the closure body
    /// classifiable from the signature alone — no
    /// ClosureReturnTypeUnknown.
    #[test]
    fn v1_5_strict_closure_declared_return_silent() {
        let tree = analyze_str(
            r#"
            { f: (Int n) -> Int => mystery(n) }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ClosureReturnTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: head-unresolved reference in strict mode produces
    /// `UnknownReferenceType { path: [head] }` alongside the warning-
    /// level `UnresolvedReference`.
    #[test]
    fn v1_5_strict_head_unresolved_escalation() {
        let tree = analyze_str(
            r#"
            { x: mystery }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::UnknownReferenceType { name, path, .. }
                if name == "mystery" && path == &vec!["mystery".to_string()])
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// Reverse: `#relaxed` keeps the warning-level UnresolvedReference
    /// and does NOT push UnknownReferenceType.
    #[test]
    fn relaxed_head_unresolved_no_unknown_ref_type() {
        let tree = analyze_str(
            r#"#relaxed
            { x: mystery }"#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::UnknownReferenceType { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: `Any`-typed `#main` param now reports
    /// `ExplicitAnyForbidden` in every mode (strict and non-strict).
    /// Replaces the v1.5 `StrictForbidsUntypedMainParam` (which only
    /// fired under strict). The new diagnostic carries a `context`
    /// string so the user knows where the ban triggered.
    #[test]
    fn v1_6_main_param_any_flagged_under_strict() {
        let tree = analyze_str(
            r#"
            #main(Any x) -> Int
            1
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("#main parameter") && context.contains("`x`"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: same ban applies under non-strict — `Any` is
    /// retired from user code globally.
    #[test]
    fn v1_6_main_param_any_flagged_non_strict() {
        let tree = analyze_str(
            r#"
            #main(Any x) -> Int
            1
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("#main parameter"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 reverse: typed main param is silent in both modes.
    #[test]
    fn v1_6_main_param_typed_silent_under_strict() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> Int
            x + 1
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: list element under strict mode whose value can't
    /// be classified (FnCall without sig). ExpressionTypeUnknown pinned on
    /// the element.
    #[test]
    fn v1_5_strict_list_element_uninferable() {
        let tree = analyze_str(
            r#"
            [1, mystery_call(), 3]
            "#,
        );
        let il = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
                if reason.contains("list element"))
        });
        assert!(il >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 reverse: list of literals is silent.
    #[test]
    fn v1_5_strict_list_of_literals_silent() {
        let tree = analyze_str(
            r#"
            [1, 2, 3]
            "#,
        );
        let il = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { .. })
        });
        assert_eq!(il, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: untyped dict value with opaque expression →
    /// ExpressionTypeUnknown.
    #[test]
    fn v1_5_strict_dict_value_uninferable() {
        let tree = analyze_str(
            r#"
            { x: mystery_fn() }
            "#,
        );
        let il = count(&tree, |d| {
            matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
                if reason.contains("dict field"))
        });
        assert!(il >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: comprehension whose iterable is `range(...)` and
    /// element body refers to `o.id` — inference should flow from
    /// `Order { Int id }` so the element type is Int.
    #[test]
    fn v1_5_strict_comprehension_uses_main_param_path() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, Float total: * }
            #main(Order o) -> List<Int>
            [x + o.id for x in range(o.id)]
            "#,
        );
        let mm = count(&tree, |d| {
            matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
        });
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: spread used as a function-call argument evaluates
    /// to its inner — `Expr::Spread` infers identically to the inner
    /// expression. Used here as a smoke test that the new
    /// `Expr::Spread` arm doesn't regress sibling-callable inference.
    #[test]
    fn v1_5_spread_expr_inference_smoke() {
        // Construct a Spread node directly via the parser path: a
        // typed binding whose value is `(...e) where { e: [1,2,3] }`
        // wouldn't actually exercise the arm because parser gates
        // `where`-bindings to dict literals. Instead assert on a
        // simpler shape: an empty `[]` is `List<Any>` and a literal
        // `[1, 2, 3]` infers as `List<Int>`. The Spread arm is
        // covered indirectly by spread-extension fixtures (where
        // `...x.y` flows through `Expr::Spread → infer_type(inner)`).
        let tree = analyze_str(
            r#"
            { List<Int> xs: [1, 2, 3] }
            "#,
        );
        let stm = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(stm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 forward: FnCall with multi-segment alias.method now goes
    /// through `lookup_signature_path`. We can't easily fixture-test
    /// the cross-module case at the unit level, but the path-aware
    /// lookup should *not* false-flag a sibling-dict-literal FnCall
    /// that the v1.0 walker was already handling. (Regression guard.)
    #[test]
    fn v1_5_sibling_method_call_still_typechecks() {
        let tree = analyze_str(
            r#"
            {
              ns: {
                add: (Int a, Int b) -> Int => a + b
              },
              Int sum: ns.add(1, 2)
            }
            "#,
        );
        let stm = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(stm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 boundary: spread of a typed sibling variable plus a
    /// strict-mode typed closure — the v1.4 path-spread + v1.5
    /// closure-strict checks coexist without false-flags.
    #[test]
    fn v1_5_strict_path_spread_after_typed_closure_silent() {
        let tree = analyze_str(
            r#"
            #schema Extras { Int a: *, Int b: * }
            {
              Extras src: { a: 1, b: 2 },
              build: (Int seed) -> Int => seed + 1,
              ...src
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::SpreadSourceTypeUnknown { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 boundary: `where`-binding's value is a list literal that
    /// itself contains an inferable element — body refers to the
    /// binding and assembles a list of lists.
    #[test]
    fn v1_5_where_nested_list_body() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> List<Int>
            xs where { List<Int> xs: [x, x + 1, x + 2] }
            "#,
        );
        let mm = count(&tree, |d| {
            matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
        });
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 boundary: comprehension element is a path that walks two
    /// hops (`o.customer.name`); strict + path-tail combine to
    /// derive `String` element, matching the typed `List<String>`
    /// slot.
    #[test]
    fn v1_5_strict_comprehension_path_two_hops_silent() {
        let tree = analyze_str(
            r#"
            #schema Customer { String name: * }
            #schema Order { Customer customer: *, Int id: * }
            #main(Order o) -> List<String>
            [o.customer.name for x in range(o.id)]
            "#,
        );
        let mm = count(&tree, |d| {
            matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
        });
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }

    // ============= v1.6: ban Any from user space =============

    /// v1.6 forward: typed binding `Any field: ...` rejected (every mode).
    #[test]
    fn v1_6_ban_typed_binding_any() {
        let tree = analyze_str(r#"{ Any payload: 42 }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("typed binding"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: nested `Any` inside `List<...>` is also flagged.
    #[test]
    fn v1_6_ban_nested_list_any() {
        let tree = analyze_str(r#"{ List<Any> xs: [1, 2, 3] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: nested `Any` inside `Dict<String, ...>` flagged.
    #[test]
    fn v1_6_ban_nested_dict_any() {
        let tree = analyze_str(r#"{ Dict<String, Any> kv: { a: 1 } }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: closure parameter typed `Any` flagged.
    #[test]
    fn v1_6_ban_closure_param_any() {
        let tree = analyze_str(r#"{ f: (Any n) -> Int => 1 }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("closure parameter"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: closure declared `-> Any` flagged.
    #[test]
    fn v1_6_ban_closure_return_any() {
        let tree = analyze_str(r#"{ f: (Int n) -> Any => n }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("closure return"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: schema field typed `Any` flagged.
    #[test]
    fn v1_6_ban_schema_field_any() {
        let tree = analyze_str(
            r#"
            #schema Outer { Any payload: * }
            { x: 1 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("schema field"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: `#main(...) -> Any` flagged on the return type.
    #[test]
    fn v1_6_ban_main_return_any() {
        let tree = analyze_str(
            r#"
            #main(Int n) -> Any
            n + 1
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("#main return"))
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 reverse: a fully concrete program does NOT trigger
    /// ExplicitAnyForbidden.
    #[test]
    fn v1_6_ban_all_concrete_silent() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: *, String name: * }
            #main(Order o) -> Int
            {
              Int id: o.id,
              bump: (Int n) -> Int => n + 1,
              Int doubled: bump(o.id) + bump(o.id)
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: stdlib `_dict_values<V>(Dict<String, V>) ->
    /// List<V>` now flows V through.
    #[test]
    fn v1_6_stdlib_dict_values_flows_v_through() {
        let tree = analyze_str(
            r#"
            {
              Dict<String, Int> scores: { math: 100, art: 90 },
              List<Int> values: _dict_values(scores)
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: stdlib `ensure.int<T>(value, message?) -> T` now
    /// preserves the input type.
    #[test]
    fn v1_6_stdlib_ensure_int_preserves_t() {
        let tree = analyze_str(
            r#"
            #main(Int x) -> Int
            n + 1 where { Int n: ensure.int(x) }
            "#,
        );
        let mm = count(&tree, |d| {
            matches!(d, Diagnostic::MainReturnTypeMismatch { .. })
        });
        assert_eq!(mm, 0, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: stdlib `len<T>(T) -> Int`. Param T is unbound;
    /// no diagnostics for `len("hello")`.
    #[test]
    fn v1_6_stdlib_len_unbound_t() {
        let tree = analyze_str(
            r#"
            {
              s: "hello",
              Int n: len(s)
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(
                d,
                Diagnostic::StaticTypeMismatch { .. } | Diagnostic::FnCallArgTypeMismatch { .. }
            )
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: stdlib `_dict_merge<V>` — uniform-V binds `V →
    /// Int` and the return stays `Dict<String, Int>`.
    #[test]
    fn v1_6_stdlib_dict_merge_uniform_v() {
        let tree = analyze_str(
            r#"
            {
              Dict<String, Int> a: { x: 1 },
              Dict<String, Int> b: { y: 2 },
              Dict<String, Int> merged: _dict_merge(a, b)
            }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: `Any` in `#schema` field caught when nested in a
    /// parameterized container.
    #[test]
    fn v1_6_ban_schema_nested_any() {
        let tree = analyze_str(
            r#"
            #schema Bag { List<Any> items: * }
            { x: 1 }
            "#,
        );
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::ExplicitAnyForbidden { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    // ============= v1.7: Tuple types =============

    /// v1.7 forward: tuple-typed binding accepts a list literal of
    /// matching arity / element types.
    #[test]
    fn v1_7_tuple_typed_binding_silent() {
        let tree = analyze_str(r#"{ (Int, String) row: [42, "Alice"] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.7 forward: tuple slot rejects element-type mismatch.
    #[test]
    fn v1_7_tuple_element_type_mismatch() {
        let tree = analyze_str(r#"{ (Int, String) row: [42, 99] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.7 forward: tuple slot rejects arity mismatch.
    #[test]
    fn v1_7_tuple_arity_mismatch() {
        let tree = analyze_str(r#"{ (Int, String) row: [42, "x", true] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.7 forward: 1-tuple syntax `(T,)` works.
    #[test]
    fn v1_7_one_tuple_silent() {
        let tree = analyze_str(r#"{ (Int,) singleton: [42] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.7 forward: unit tuple `()` accepts only an empty list.
    #[test]
    fn v1_7_unit_tuple_silent() {
        let tree = analyze_str(r#"{ () unit: [] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.7 forward: heterogeneous list under typed `List<T>` slot
    /// still uses the per-element walker. The tuple inference
    /// preserves precise element types so each mismatch reports its
    /// exact position.
    #[test]
    fn v1_7_heterogeneous_list_under_int_slot_per_element_diagnostics() {
        let tree = analyze_str(r#"{ List<Int> xs: [1, "x", 3] }"#);
        let n = count(
            &tree,
            |d| matches!(d, Diagnostic::StaticTypeMismatch { field, .. } if field == "xs[1]"),
        );
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.7 forward: tuple inside `List<...>` (list of tuples) — each
    /// row is a fixed-shape tuple.
    #[test]
    fn v1_7_list_of_tuples_silent() {
        let tree = analyze_str(r#"{ List<(String, Int)> entries: [["Alice", 1], ["Bob", 2]] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.7 forward: nested tuple positional mismatch surfaces with
    /// the precise element index.
    #[test]
    fn v1_7_list_of_tuples_inner_mismatch() {
        let tree =
            analyze_str(r#"{ List<(String, Int)> entries: [["Alice", 1], ["Bob", "two"]] }"#);
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::StaticTypeMismatch { .. })
        });
        assert!(n >= 1, "{:?}", tree.diagnostics);
    }

    /// v1.7 boundary: `(Int)` (no trailing comma) is *not* parsed as
    /// a tuple — it's not a valid type position any more (v1.7
    /// reserves parenthesized type syntax for tuples). The parser
    /// rejects it; this test guards the rejection by checking the
    /// fall-through path for method shorthand `f(x):` still works
    /// (which depends on `(...)` not being claimed by the tuple
    /// branch of `parse_type_node`).
    #[test]
    fn v1_7_method_shorthand_still_parses() {
        let tree = analyze_str(r#"{ helper(): 1, x: helper() }"#);
        // No UnresolvedReference / parse failure.
        let n = count(&tree, |d| {
            matches!(d, Diagnostic::UnresolvedReference { .. })
        });
        assert_eq!(n, 0, "{:?}", tree.diagnostics);
    }

    /// v1.5 Any-coverage audit: every "I-couldn't-infer" silent-Any
    /// site under strict mode produces at least one error-severity
    /// diagnostic. This is the regression guard for the user-visible
    /// invariant "strict mode never produces an opaque `Any` type".
    #[test]
    fn v1_5_strict_any_coverage_audit() {
        let tree = analyze_str(
            r#"
            #schema Order { Int id: * }
            #main(Order o) -> Dict
            {
              bad_list: [mystery(), 1, 2],
              bad_closure: (Int n) => mystery(n),
              bad_path: o.unknown,
              untyped_closure: (n) => n + 1,
            }
            "#,
        );
        // Every "Any leak" site we documented in v1.5 must surface a
        // strict diagnostic. Use disjoint predicates so a single fix
        // can't accidentally cover a different leak.
        assert!(
            count(
                &tree,
                |d| matches!(d, Diagnostic::ExpressionTypeUnknown { reason, .. }
                if reason.contains("list element"))
            ) >= 1,
            "list element ExpressionTypeUnknown missing: {:?}",
            tree.diagnostics
        );
        assert!(
            count(&tree, |d| matches!(
                d,
                Diagnostic::ClosureReturnTypeUnknown { .. }
            )) >= 1,
            "closure body ClosureReturnTypeUnknown missing: {:?}",
            tree.diagnostics
        );
        assert!(
            count(&tree, |d| matches!(
                d,
                Diagnostic::UnknownReferenceType { name, .. } if name == "unknown"
            )) >= 1,
            "path-tail UnknownReferenceType missing: {:?}",
            tree.diagnostics
        );
        assert!(
            count(&tree, |d| matches!(
                d,
                Diagnostic::ClosureParamTypeMissing { param_name, .. }
                    if param_name == "n"
            )) >= 1,
            "untyped param ClosureParamTypeMissing missing: {:?}",
            tree.diagnostics
        );
    }
}
