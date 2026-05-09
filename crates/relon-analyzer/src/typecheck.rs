//! Static type-check pass.
//!
//! Two complementary checks, both emitted as `Warning`-severity
//! diagnostics so the evaluator's authoritative runtime checks remain
//! the source of truth:
//!
//! * [`UnresolvedReference`] — a `&sibling.X` / `Variable(X)` whose
//!   head couldn't be statically bound *and* no spread / closure
//!   binding on the active scope chain could plausibly save it.
//! * [`StaticTypeMismatch`] — a typed schema binding whose value
//!   expression has a determinable shape (literal, list, dict, type)
//!   that disagrees with the field's declared type.
//!
//! [`UnresolvedReference`]: crate::Diagnostic::UnresolvedReference
//! [`StaticTypeMismatch`]: crate::Diagnostic::StaticTypeMismatch

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
    let base_index = build_base_index(tree);

    let mut pipe_target_calls = HashSet::new();
    collect_pipe_target_calls(root, &mut pipe_target_calls);

    let mut walker = Walker {
        tree,
        scope_stack: Vec::new(),
        schema_index,
        enum_index,
        base_index,
        pipe_target_calls,
    };
    walker.visit(root);
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
        }

        match &*node.expr {
            Expr::Dict(pairs) => {
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
                    if let (Some(name), Expr::Closure { .. }) = (field_name, &*value.expr) {
                        self.tree
                            .field_closure_index
                            .insert(name.to_string(), value.id);
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
                }
                self.visit_internal(body, None);
                self.scope_stack.pop();
            }
            Expr::List(items) => {
                for item in items {
                    self.visit_internal(item, None);
                }
            }
            Expr::Reference { base, path } => {
                self.check_unresolved_ref(node, base, path);
            }
            Expr::Variable(path) => {
                self.check_unresolved_var(node, path);
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
                if arg_ty.subsumes_with(&expected_ty, Some(&self.base_index)) {
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
        // Multi-segment v1: limit to dict-literal sibling closure.
        if path.len() != 2 {
            return None;
        }
        let TokenKey::String(method, _, _) = &path[1] else {
            return None;
        };
        // Try sibling-field dict-literal first (Stage 3.6). Falls
        // through to the v1.1 cross-module index when there's no
        // sibling field with that name.
        if let Some(target) = self.lookup_field_node(head) {
            // Only walk literal dicts — abstract types (FnCall /
            // Reference / typed schema bindings) silently fall through
            // to runtime.
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
        // v1.1: cross-module alias.method — `#import alias from "lib"`
        // exposes the imported module's top-level closures under
        // `alias.method`.
        if let Some(idx) = self.tree.workspace_import_index.as_ref() {
            if let Some(methods) = idx.aliased_closures.get(head) {
                if let Some(sig) = methods.get(method) {
                    return Some(sig.clone());
                }
            }
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
        for (pat, body) in arms {
            if matches!(pat.expr.as_ref(), Expr::Wildcard) {
                continue;
            }
            // If any non-wildcard arm body is uninferrable, defer to
            // runtime — we'd be guessing.
            let Some(t) = infer_type(body, &scope) else {
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
        for param in params {
            let ty = param
                .type_hint
                .as_ref()
                .map(infer::infer_from_type_node)
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
        if body_ty.subsumes_with(declared_return, Some(&self.base_index)) {
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
            name,
            range: span_of(node.range),
        });
    }

    fn check_unresolved_var(&mut self, node: &Node, path: &[TokenKey]) {
        if self.tree.references.contains_key(&node.id) {
            // Same Stage 2.6 tail walk as `check_unresolved_ref`.
            self.check_path_tail(node, path);
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
            name,
            range: span_of(node.range),
        });
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
            if t.subsumes_with(expected, Some(&self.base_index)) {
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
        }
    }

    /// Recursively check the contents of List and Dict literals against
    /// expected generic parameters.
    fn check_generics(&mut self, expected: &TypeNode, value: &Node, field_name: &str) {
        if expected.generics.is_empty() {
            return;
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
            }
            Expr::Dict(pairs) => {
                if expected.path == vec!["Dict"] && expected.generics.len() == 2 {
                    // We only check if Key is String for now (common case)
                    if expected.generics[0].path == vec!["String"] {
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
        let mut to_check: Vec<(String, TypeNode, &Node)> = Vec::new();
        if let Some(field_types) = self.schema_index.get(schema_name) {
            for (key, inner) in pairs {
                if let TokenKey::String(field_name, _, _) = key {
                    if let Some(field_type) = field_types.get(field_name) {
                        to_check.push((field_name.clone(), field_type.clone(), inner));
                    }
                }
            }
        }
        for (field_name, field_type, inner) in to_check {
            self.check_typed_binding(&field_type, inner, &field_name);
        }
    }
}

// `static_type_of` / `matches_expected` were the pre-Stage-1.2
// String-name inference helpers. They've been fully replaced by the
// `infer::infer_type` engine and `InferredType::subsumes`, and the
// last in-tree caller migrated in Stage 1.4. The legacy helpers are
// removed here; consumers that need the old name-string view can
// build it from `InferredType::name`.

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
        (InferredType::List(_), "List") | (InferredType::Dict(_), "Dict")
    )
}

fn format_type(t: &TypeNode) -> String {
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
}
