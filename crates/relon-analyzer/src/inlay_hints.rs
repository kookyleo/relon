//! Inlay hints — ghost text rendered inline in the editor.
//!
//! Only emits parameter-name hints at positional call sites today:
//! `currency(100, "USD")` becomes `currency(val: 100, symbol: "USD")`
//! visually, while the source stays unchanged. Named args
//! (`currency(val: 100, symbol: "USD")`) are skipped — the user already
//! wrote the label.
//!
//! Hints are computed once per module from `(root, tree)`; the caller
//! is expected to re-run after edits. Each hint carries an LSP-style
//! `(line, character)` so IDEs / playgrounds can paint a widget
//! decoration without re-walking the source.

use crate::sig::{lookup_signature_path, FnSignature};
use crate::tree::AnalyzedTree;
use relon_parser::{CallArg, Expr, Node};
use std::collections::HashMap;

/// One ghost-text hint. `kind` is hard-coded to "parameter" today but
/// is carried in the wire format so we can extend (return-type hints,
/// inferred-type hints) without breaking callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub line: u32,
    pub character: u32,
    pub offset: usize,
    pub label: String,
    pub kind: InlayHintKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlayHintKind {
    Parameter,
}

/// Walk every `Expr::FnCall` in `root` and emit one hint per positional
/// argument when we can resolve a signature that names the slot.
pub fn collect(root: &Node, tree: &AnalyzedTree) -> Vec<InlayHint> {
    let host: HashMap<String, FnSignature> = HashMap::new();
    let mut out = Vec::new();
    visit(root, tree, &host, &mut out);
    out.sort_by_key(|h| h.offset);
    out
}

fn visit(
    node: &Node,
    tree: &AnalyzedTree,
    host: &HashMap<String, FnSignature>,
    out: &mut Vec<InlayHint>,
) {
    if let Expr::FnCall { path, args } = &*node.expr {
        emit_for_call(path, args, tree, host, out);
    }
    for child in relon_parser::child_nodes(node) {
        visit(child, tree, host, out);
    }
}

fn emit_for_call(
    path: &[relon_parser::TokenKey],
    args: &[CallArg],
    tree: &AnalyzedTree,
    host: &HashMap<String, FnSignature>,
    out: &mut Vec<InlayHint>,
) {
    let name_segments: Vec<String> = path
        .iter()
        .filter_map(|seg| match seg {
            relon_parser::TokenKey::String(s, _, _) => Some(s.clone()),
            _ => None,
        })
        .collect();
    if name_segments.is_empty() {
        return;
    }
    let sig = match lookup_signature_path(&name_segments, tree, host) {
        Some(s) => s,
        None => return,
    };
    for (i, arg) in args.iter().enumerate() {
        // Skip named args — the source already labels them, painting a
        // hint would shout "val: val: 100" at the reader.
        if arg.name.is_some() {
            continue;
        }
        // Out-of-range slot: extra positional past the param list
        // (variadic tail) or wrong arity. Either way no name to render.
        let Some(param) = sig.params.get(i) else {
            continue;
        };
        let range = arg.value.range;
        out.push(InlayHint {
            line: range.start.line,
            character: range.start.column as u32,
            offset: range.start.offset,
            label: format!("{}:", param.name),
            kind: InlayHintKind::Parameter,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    fn collect_for(src: &str) -> Vec<InlayHint> {
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        collect(&root, &tree)
    }

    #[test]
    fn labels_positional_args_of_stdlib_call() {
        // `len(items)` resolves to the stdlib `len(value: List<T>)` —
        // we should see one hint labelled `value:` at the arg start.
        let src = r#"len([1, 2, 3])"#;
        let hints = collect_for(src);
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert_eq!(hints[0].label, "value:");
    }

    #[test]
    fn labels_user_closure_positional_args() {
        let src = r#"{
                add: (Int x, Int y) -> Int => x + y,
                Int z: add(1, 2)
            }"#;
        let hints = collect_for(src);
        let labels: Vec<&str> = hints.iter().map(|h| h.label.as_str()).collect();
        assert!(labels.contains(&"x:"), "labels: {labels:?}");
        assert!(labels.contains(&"y:"), "labels: {labels:?}");
    }

    #[test]
    fn no_hints_for_unknown_callee() {
        let src = r#"unknown_fn(1, 2)"#;
        let hints = collect_for(src);
        assert!(hints.is_empty(), "{hints:?}");
    }
}
