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
use crate::resolve::ScopeFrame;
use crate::tree::AnalyzedTree;
use relon_parser::{Expr, Node, RefBase, TokenKey, TypeNode};
use std::collections::HashMap;

/// Run the type-check walker over `root` and append diagnostics to
/// `tree`. Must be called after [`crate::resolve::resolve_references`]
/// and [`crate::schema::collect_schemas`] so the side-tables they
/// produce are available.
pub fn typecheck(root: &Node, tree: &mut AnalyzedTree) {
    // Collect static type info and field bindings for each declared
    // schema so the value-type pass can look fields up by name.
    let schema_index = build_schema_index(tree);

    let mut walker = Walker {
        tree,
        scope_stack: Vec::new(),
        schema_index,
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

struct Walker<'a> {
    tree: &'a mut AnalyzedTree,
    scope_stack: Vec<ScopeFrame>,
    schema_index: SchemaIndex,
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
            _ => {
                for child in iter_children(node) {
                    self.visit(child);
                }
            }
        }
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
        let Some(field_types) = self.schema_index.get(schema_name).cloned() else {
            return;
        };
        let Expr::Dict(pairs) = &*value.expr else {
            return;
        };
        for (key, inner) in pairs {
            let TokenKey::String(field_name, _, _) = key else {
                continue;
            };
            let Some(field_type) = field_types.get(field_name) else {
                continue;
            };
            self.check_typed_binding(field_type, inner, field_name);
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
        "Int" | "Float" | "Bool" | "String" | "Null" | "List" | "Dict" | "Closure" | "Fn"
        | "Enum" => exp == found || (exp == "Fn" && found == "Closure"),
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

fn build_frame(pairs: &[(TokenKey, Node)]) -> ScopeFrame {
    let mut frame = ScopeFrame::default();
    for (key, value) in pairs {
        match key {
            TokenKey::String(name, _, _) => {
                frame.fields.insert(name.clone(), value.id);
            }
            TokenKey::Spread(_) => {
                frame.has_dynamic_spread = true;
            }
            _ => {}
        }
    }
    frame
}

fn path_head(path: &[TokenKey]) -> Option<String> {
    match path.first()? {
        TokenKey::String(s, _, _) => Some(s.clone()),
        _ => None,
    }
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

/// Same shape as `crate::resolve::iter_children`, copied to avoid
/// crossing module boundaries on a private helper.
fn iter_children(node: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    match &*node.expr {
        Expr::Dict(pairs) => {
            for (_, value) in pairs {
                out.push(value);
            }
        }
        Expr::List(items) => {
            for item in items {
                out.push(item);
            }
        }
        Expr::Spread(inner) => out.push(inner),
        Expr::Comprehension {
            element,
            iterable,
            condition,
            ..
        } => {
            out.push(element);
            out.push(iterable);
            if let Some(cond) = condition {
                out.push(cond);
            }
        }
        Expr::Binary(_, l, r) => {
            out.push(l);
            out.push(r);
        }
        Expr::Unary(_, inner) => out.push(inner),
        Expr::Ternary { cond, then, els } => {
            out.push(cond);
            out.push(then);
            out.push(els);
        }
        Expr::FnCall { args, .. } => {
            for arg in args {
                out.push(&arg.value);
            }
        }
        Expr::FString(parts) => {
            for part in parts {
                if let relon_parser::FStringPart::Interpolation(n) = part {
                    out.push(n);
                }
            }
        }
        Expr::Where { expr, bindings } => {
            out.push(expr);
            out.push(bindings);
        }
        Expr::Match { expr, arms } => {
            out.push(expr);
            for (pat, body) in arms {
                out.push(pat);
                out.push(body);
            }
        }
        Expr::Closure { body, .. } => out.push(body),
        _ => {}
    }
    out
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
}
