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
use crate::resolve::{build_frame, path_head, ScopeFrame};
use crate::tree::AnalyzedTree;
use relon_parser::{
    child_nodes, is_builtin_type_name, Expr, Node, RefBase, TokenKey, TokenRange, TypeNode,
};
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
/// inner field against `User`'s schema in one pass.
type SchemaIndex = HashMap<String, HashMap<String, TypeNode>>;

fn build_schema_index(tree: &AnalyzedTree) -> SchemaIndex {
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
        // Type binding check first — `Type field: value` carries info
        // that lets us validate `value` immediately, before recursing.
        if let Some(t) = &node.type_hint {
            self.check_typed_binding(t, node, /*field_name=*/ "_");
        }

        match &*node.expr {
            Expr::Dict(pairs) => {
                let frame = build_frame(pairs);
                self.scope_stack.push(frame);
                for (key, value) in pairs {
                    if let TokenKey::String(field_name, _, _) = key {
                        if let Some(t) = &value.type_hint {
                            self.check_typed_binding(t, value, field_name);
                        }
                        // If the value's type-hint is a custom schema,
                        // also walk the value's dict fields against
                        // the schema's expected types.
                        if let Some(t) = &value.type_hint {
                            self.check_against_custom_schema(t, value);
                        }
                    }
                    self.visit(value);
                }
                self.scope_stack.pop();
            }
            Expr::Closure { params, body, .. } => {
                let mut frame = ScopeFrame::default();
                for param in params {
                    frame.closure_params.insert(param.name.clone(), body.id);
                }
                self.scope_stack.push(frame);
                self.visit(body);
                self.scope_stack.pop();
            }
            Expr::List(items) => {
                for item in items {
                    self.visit(item);
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
                self.visit(expr);
                for (pat, body) in arms {
                    self.visit(pat);
                    self.visit(body);
                }
            }
            _ => {
                for child in child_nodes(node) {
                    self.visit(child);
                }
            }
        }
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
        let Some(found) = static_type_of(value) else {
            return;
        };
        if matches_expected(expected, &found) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
            field: field_name.to_string(),
            expected: format_type(expected),
            found,
            range: span_of(value.range),
        });
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

/// Return the static type-name of `node` if it can be classified
/// without evaluating. Returns `None` for any expression whose value
/// depends on runtime computation (FnCall, Reference, Binary on
/// non-trivial operands, ...).
fn static_type_of(node: &Node) -> Option<String> {
    match &*node.expr {
        Expr::Null => Some("Null".to_string()),
        Expr::Bool(_) => Some("Bool".to_string()),
        Expr::Int(_) => Some("Int".to_string()),
        Expr::Float(_) => Some("Float".to_string()),
        Expr::String(_) => Some("String".to_string()),
        Expr::List(_) => Some("List".to_string()),
        Expr::Dict(_) => Some("Dict".to_string()),
        Expr::Closure { .. } => Some("Closure".to_string()),
        _ => None,
    }
}

/// Cheap structural match of a static `found` type-name against the
/// declared `expected` `TypeNode`. Doesn't try to be exhaustive — only
/// catches the obvious "I declared `Int` but you wrote a String"
/// kinds of mistakes. Generic parameters (`List<Int>`) and union
/// types (`Enum<...>`) are intentionally skipped because verifying
/// them right requires walking the value's interior, which the
/// runtime already does well.
fn matches_expected(expected: &TypeNode, found: &str) -> bool {
    if expected.path.len() != 1 {
        // Multi-segment custom types — defer to runtime / the
        // dedicated `check_against_custom_schema` walk.
        return true;
    }
    let exp = expected.path[0].as_str();
    if expected.is_optional && found == "Null" {
        return true;
    }
    match exp {
        "Any" => true,
        "Number" => matches!(found, "Int" | "Float"),
        // Built-in primitives must match exactly.
        _ if is_builtin_type_name(exp) => exp == found || (exp == "Fn" && found == "Closure"),
        // Custom schema name: defer to `check_against_custom_schema`.
        _ => true,
    }
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
                @schema User: { String name: *, Int age: * },
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
                @schema N: Enum<A { x: Int }, B { y: Int }, C>,
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
                @schema N: Enum<Email { x: Int }, SMS { y: Int }>,
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
                @schema N: Enum<A { x: Int }, B { y: Int }>,
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
                @schema N: Enum<A { x: Int }, B { y: Int }, C>,
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
                @schema N: Enum<A { x: Int }, B { y: Int }>,
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
}
