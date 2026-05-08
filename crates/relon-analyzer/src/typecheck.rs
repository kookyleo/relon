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
use crate::infer::{self, infer_type, InferredType, TypeScope};
use crate::resolve::{build_frame, path_head, ScopeFrame};
use crate::tree::AnalyzedTree;
use relon_parser::{child_nodes, Expr, Node, RefBase, TokenKey, TokenRange, TypeNode};
use std::collections::{HashMap, HashSet};

/// Run the type-check walker over `root` and append diagnostics to
/// `tree`. Must be called after [`crate::resolve::resolve_references`]
/// and [`crate::schema::collect_schemas`] so the side-tables they
/// produce are available.
pub fn typecheck(root: &Node, tree: &mut AnalyzedTree) {
    // Collect static type info and field bindings for each declared
    // schema so the value-type pass can look fields up by name.
    let schema_index = build_schema_index(tree);
    let enum_index = build_enum_index(tree);

    let mut walker = Walker {
        tree,
        scope_stack: Vec::new(),
        schema_index,
        enum_index,
    };
    walker.visit(root);
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

struct Walker<'a> {
    tree: &'a mut AnalyzedTree,
    scope_stack: Vec<ScopeFrame>,
    schema_index: SchemaIndex,
    enum_index: EnumIndex,
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
                self.visit_internal(left, None);
                self.visit_internal(right, None);
            }
            _ => {
                for child in child_nodes(node) {
                    self.visit_internal(child, None);
                }
            }
        }
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
        if body_ty.subsumes(declared_return) {
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
            return;
        }
        let Some(name) = path_head(path) else { return };
        // Variables also resolve against function names registered
        // by the host (stdlib like `range`, `len`, ...). The analyzer
        // can't know about host registrations, so unresolved
        // variables are warnings only when no dynamic frame might
        // save them and the name isn't a known stdlib symbol.
        if self.dynamic_save(&name) || is_likely_stdlib(&name) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
            name,
            range: span_of(node.range),
        });
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
            if t.subsumes(expected) {
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

/// Names registered by the evaluator's stdlib + commonly-used
/// host-supplied helpers. Conservative — we'd rather miss a real
/// unresolved warning than spam the user with false positives on
/// well-known names.
fn is_likely_stdlib(name: &str) -> bool {
    matches!(
        name,
        "range"
            | "len"
            | "list"
            | "dict"
            | "string"
            | "math"
            | "is"
            | "value"
            | "ensure"
            | "abs"
            | "min"
            | "max"
            | "sum"
            | "format"
            | "type_of"
    )
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
    /// call (not statically classifiable), the analyzer must stay
    /// silent — runtime keeps owning the verdict.
    #[test]
    fn does_not_flag_fncall_sibling_reference() {
        let tree = analyze_str(
            r#"{
                xs: range(0, 10),
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
}
