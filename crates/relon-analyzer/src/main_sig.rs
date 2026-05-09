//! `#main(<type> <ident>, ...) [-> <type>]` collection pass.
//!
//! The `#main(...)` directive declares the file as an **entry program**
//! whose host-pushed arguments must validate against the listed
//! parameters. Every parameter becomes a root-scope local available
//! directly by name (no `input.` prefix). A file without `#main` is a
//! library / static config — importable, evaluable as a `Value`, but
//! not a host-entry. The optional `-> Type` clause declares the
//! expected return type; when absent the entry's return value is left
//! unchecked.
//!
//! This pass walks the root document's directives, picks up at most
//! one `#main(...)` declaration, and stores it in
//! [`AnalyzedTree::main_signature`]. Multiple declarations and
//! parameters missing types are surfaced as analyzer diagnostics.

use crate::diagnostic::{span_of, Diagnostic};
use crate::directive_names::MAIN;
use crate::tree::AnalyzedTree;
use relon_parser::{is_builtin_type_name, DirectiveBody, Node, TokenRange, TypeNode};

/// One `<type> <ident>` parameter declared on `#main(...)`.
#[derive(Debug, Clone)]
pub struct MainParam {
    /// Parameter name as used in the body (e.g. `${u.name}`).
    pub name: String,
    /// Declared type. Validated against the host-pushed value at
    /// `Evaluator::run_main` time.
    pub type_node: TypeNode,
    /// Source range of the parameter (for diagnostics).
    pub range: TokenRange,
}

/// Parsed `#main(...)` signature attached to the root node.
#[derive(Debug, Clone)]
pub struct MainSignature {
    /// Parameters in declaration order; the host may push them in any
    /// order (lookup is by name).
    pub params: Vec<MainParam>,
    /// Optional return type declared via `-> Type` after the parameter
    /// list. `None` means the entry's return value is left unchecked.
    pub return_type: Option<TypeNode>,
    /// Source range of the entire `#main(...)` directive.
    pub range: TokenRange,
}

/// Walk the root node's directives and pick up the `#main(...)`
/// signature, if any. At most one declaration is allowed; subsequent
/// ones produce [`Diagnostic::DuplicateMainDirective`]. Each parameter
/// must declare a type — the directive parser already enforces the
/// `<ident> : <type>` shape, so this pass primarily handles the "more
/// than one #main" case.
pub fn collect_main(root: &Node, tree: &mut AnalyzedTree) {
    let mut first: Option<TokenRange> = None;
    for dir in &root.directives {
        if dir.name != MAIN {
            continue;
        }
        let DirectiveBody::Main {
            params: dir_params,
            return_type,
        } = &dir.body
        else {
            continue;
        };
        if let Some(first_range) = first {
            tree.diagnostics.push(Diagnostic::DuplicateMainDirective {
                first: span_of(first_range),
                second: span_of(dir.range),
            });
            continue;
        }
        first = Some(dir.range);

        let params: Vec<MainParam> = dir_params
            .iter()
            .map(|p| MainParam {
                name: p.name.clone(),
                type_node: p.type_node.clone(),
                range: p.name_range,
            })
            .collect();
        tree.main_signature = Some(MainSignature {
            params,
            return_type: return_type.clone(),
            range: dir.range,
        });
    }
    // Stage 1.8: every #main param's declared type head must be
    // resolvable to either a builtin or a user-declared schema. Run
    // after the loop so we use the fully-resolved schema set; the
    // schema pass populates `tree.schemas` before us.
    check_unknown_param_types(tree);
    check_unknown_return_type(tree);
    // v1.6: ban `Any` (and any nested `Any` inside generics) in
    // every `#main(...)` parameter type and in the return-type
    // annotation. Replaces the v1.5 strict-only `Any` head check —
    // `Any` is now retired from the user-facing surface entirely,
    // not just under `#strict`.
    check_ban_any_main_signature(tree);
}

/// v1.6: scan every `#main(...)` parameter type and the optional
/// return type for `Any` and push [`Diagnostic::ExplicitAnyForbidden`]
/// on each occurrence. The walk is recursive so `List<Any>` /
/// `Dict<String, Any>` / `Result<Any, Err>` are all caught.
fn check_ban_any_main_signature(tree: &mut AnalyzedTree) {
    let Some(sig) = tree.main_signature.as_ref() else {
        return;
    };
    let mut to_emit: Vec<Diagnostic> = Vec::new();
    for p in &sig.params {
        crate::ban_unsafe_types::scan_typenode_for_any(
            &p.type_node,
            &format!("#main parameter `{}`", p.name),
            &mut to_emit,
        );
    }
    if let Some(rt) = sig.return_type.as_ref() {
        crate::ban_unsafe_types::scan_typenode_for_any(rt, "#main return type", &mut to_emit);
    }
    tree.diagnostics.extend(to_emit);
}

/// Push `UnknownTypeName` for any `#main` parameter whose declared
/// single-segment type head isn't a builtin or a user-declared schema.
/// Multi-segment paths (`pkg.Type`) and known-builtin / user-schema
/// names are left alone.
fn check_unknown_param_types(tree: &mut AnalyzedTree) {
    let Some(sig) = tree.main_signature.as_ref() else {
        return;
    };
    let unknown: Vec<Diagnostic> = sig
        .params
        .iter()
        .filter_map(|p| unknown_type_diagnostic(&p.type_node, tree))
        .collect();
    tree.diagnostics.extend(unknown);
}

/// Same check for the optional `-> Type` return annotation.
fn check_unknown_return_type(tree: &mut AnalyzedTree) {
    let Some(sig) = tree.main_signature.as_ref() else {
        return;
    };
    let Some(return_type) = sig.return_type.as_ref() else {
        return;
    };
    if let Some(d) = unknown_type_diagnostic(return_type, tree) {
        tree.diagnostics.push(d);
    }
}

fn unknown_type_diagnostic(t: &TypeNode, tree: &AnalyzedTree) -> Option<Diagnostic> {
    // Single-segment: original v1 check (builtin / prelude / declared).
    if t.path.len() == 1 {
        let head = &t.path[0];
        if is_builtin_type_name(head) {
            return None;
        }
        if matches!(head.as_str(), "Result" | "Option") {
            return None;
        }
        let known = tree
            .schemas
            .values()
            .any(|def| def.name.as_deref() == Some(head.as_str()))
            || tree.root_schemas.iter().any(|d| d.name == *head);
        if known {
            return None;
        }
        return Some(Diagnostic::UnknownTypeName {
            name: head.clone(),
            range: span_of(t.range),
        });
    }
    // v1.8+: two-segment `pkg.Tail` reaches us before the workspace
    // import index is attached. Push a tentative `UnknownTypeName` with
    // a dotted name; `re_check_unknown_types` clears it iff the entry's
    // import index resolves `head` to an alias whose exports include
    // `tail`. Otherwise the user sees the diagnostic, which is the
    // correct outcome for `#main(pkg.Wrong x)`.
    if t.path.len() == 2 {
        let dotted = format!("{}.{}", t.path[0], t.path[1]);
        return Some(Diagnostic::UnknownTypeName {
            name: dotted,
            range: span_of(t.range),
        });
    }
    None
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

    /// Stage 1.8 forward: a `#main` parameter with a non-builtin /
    /// non-declared type triggers `UnknownTypeName`.
    #[test]
    fn flags_unknown_main_param_type() {
        let tree = analyze_str(
            r#"#main(Mystery x)
            { ok: 1 }"#,
        );
        let unk: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnknownTypeName { name, .. } if name == "Mystery"))
            .collect();
        assert_eq!(unk.len(), 1, "{:?}", tree.diagnostics);
    }

    /// Stage 1.8 reverse: builtin types are silent.
    #[test]
    fn does_not_flag_builtin_main_param_type() {
        let tree = analyze_str(
            r#"#main(Int n)
            { ok: n }"#,
        );
        let unk: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnknownTypeName { .. }))
            .collect();
        assert!(unk.is_empty(), "{:?}", tree.diagnostics);
    }

    /// Stage 1.8 reverse: a user-declared root-level schema is also a
    /// known type — silent.
    #[test]
    fn does_not_flag_root_schema_main_param_type() {
        let tree = analyze_str(
            r#"#schema User { String name: * }
            #main(User u)
            { ok: u }"#,
        );
        let unk: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::UnknownTypeName { .. }))
            .collect();
        assert!(unk.is_empty(), "{:?}", tree.diagnostics);
    }

    // ============= v1.6 ban-Any in #main =============

    /// v1.6 forward: `#main(Any x)` reports `ExplicitAnyForbidden`
    /// regardless of strict-mode setting.
    #[test]
    fn v1_6_main_param_any_flagged_default_mode() {
        let tree = analyze_str(
            r#"#main(Any x) -> Int
            1"#,
        );
        let n: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("#main parameter") && context.contains("`x`"))
            })
            .collect();
        assert_eq!(n.len(), 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: `#main(...) -> Any` flagged on the return type.
    #[test]
    fn v1_6_main_return_any_flagged() {
        let tree = analyze_str(
            r#"#main(Int n) -> Any
            n"#,
        );
        let n: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| {
                matches!(d, Diagnostic::ExplicitAnyForbidden { context, .. }
                if context.contains("#main return"))
            })
            .collect();
        assert_eq!(n.len(), 1, "{:?}", tree.diagnostics);
    }

    /// v1.6 forward: nested `Any` inside a `#main` param's
    /// generic (e.g. `List<Any>`) is also flagged.
    #[test]
    fn v1_6_main_param_nested_list_any_flagged() {
        let tree = analyze_str(
            r#"#main(List<Any> xs) -> Int
            len(xs)"#,
        );
        let n: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::ExplicitAnyForbidden { .. }))
            .collect();
        assert!(!n.is_empty(), "{:?}", tree.diagnostics);
    }

    /// v1.6 reverse: a fully concrete `#main` is silent.
    #[test]
    fn v1_6_main_concrete_silent() {
        let tree = analyze_str(
            r#"#main(Int n) -> Int
            n + 1"#,
        );
        let n: Vec<_> = tree
            .diagnostics
            .iter()
            .filter(|d| matches!(d, Diagnostic::ExplicitAnyForbidden { .. }))
            .collect();
        assert!(n.is_empty(), "{:?}", tree.diagnostics);
    }
}
