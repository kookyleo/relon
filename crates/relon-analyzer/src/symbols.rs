//! Outline / "Go to symbol" support.
//!
//! Walks the top-level dict and surfaces a flat list of symbols an
//! IDE can render in its outline / file picker:
//!
//!   - `#schema X { ... }` declarations → `Schema` kind, with
//!     per-field children for the schema's body.
//!   - Top-level dict-field pairs → `Method` for closure values,
//!     `Field` for plain values, `Schema` for `User alice: { … }`
//!     style typed pairs (whose value is itself a dict).
//!
//! The flat representation (single `Vec`) is intentional: every
//! symbol carries its own `range` so the caller can build a tree by
//! containment if it needs to. Outline pickers usually want flat
//! lists anyway (fuzzy-match across the whole document).

use crate::tree::AnalyzedTree;
use relon_parser::{DirectiveBody, Expr, Node, TokenKey, TokenRange};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    /// `#schema Foo { … }` — a structural type declaration.
    Schema,
    /// Closure-valued dict pair: `add: (x, y) => x + y`.
    Method,
    /// Plain dict pair: `name: "Relon"`.
    Field,
    /// A field declared inside a `#schema` body.
    SchemaField,
    /// `#import` line.
    Import,
}

#[derive(Debug, Clone)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Full source range of the declaration (key + value).
    pub range: TokenRange,
    /// Just the identifier — what the IDE highlights / matches against.
    pub selection_range: TokenRange,
    /// Index of the parent in the returned `Vec` (None for top-level).
    /// Lets callers reconstruct a tree without walking the AST again.
    pub parent: Option<usize>,
    /// Doc comment immediately preceding the declaration, if any.
    pub doc: Option<String>,
}

/// Collect every outline-relevant symbol declared in the given root.
pub fn collect(root: &Node, _tree: &AnalyzedTree) -> Vec<DocumentSymbol> {
    let mut out = Vec::new();
    // Process top-level directives first so #schema / #import always
    // sort above their dict-field siblings in IDE outlines.
    collect_directives(root, None, &mut out);
    if let Expr::Dict(pairs) = &*root.expr {
        for (key, value) in pairs {
            collect_pair(key, value, None, &mut out);
        }
    }
    out
}

fn collect_directives(node: &Node, parent: Option<usize>, out: &mut Vec<DocumentSymbol>) {
    for dir in &node.directives {
        match &dir.body {
            DirectiveBody::NameBody {
                name,
                name_range,
                body,
                ..
            } => {
                let idx = out.len();
                out.push(DocumentSymbol {
                    name: name.clone(),
                    kind: SymbolKind::Schema,
                    range: dir.range,
                    selection_range: *name_range,
                    parent,
                    doc: node.doc_comment.clone(),
                });
                // Walk the schema body's dict for per-field children.
                if let Expr::Dict(pairs) = &*body.expr {
                    for (key, value) in pairs {
                        if let TokenKey::String(fname, krange, _) = key {
                            out.push(DocumentSymbol {
                                name: fname.clone(),
                                kind: SymbolKind::SchemaField,
                                range: combine_pair_range(*krange, value.range),
                                selection_range: *krange,
                                parent: Some(idx),
                                doc: None,
                            });
                        }
                    }
                }
            }
            DirectiveBody::Import {
                path, path_range, ..
            } => {
                out.push(DocumentSymbol {
                    name: path.clone(),
                    kind: SymbolKind::Import,
                    range: dir.range,
                    selection_range: *path_range,
                    parent,
                    doc: None,
                });
            }
            _ => {}
        }
    }
}

fn collect_pair(
    key: &TokenKey,
    value: &Node,
    parent: Option<usize>,
    out: &mut Vec<DocumentSymbol>,
) {
    let TokenKey::String(name, key_range, _) = key else {
        return;
    };
    let kind = classify_value(value);
    let idx = out.len();
    out.push(DocumentSymbol {
        name: name.clone(),
        kind,
        range: combine_pair_range(*key_range, value.range),
        selection_range: *key_range,
        parent,
        doc: value.doc_comment.clone(),
    });
    // Recurse into nested dict values so the outline mirrors structure.
    if let Expr::Dict(pairs) = &*value.expr {
        for (k, v) in pairs {
            collect_pair(k, v, Some(idx), out);
        }
    }
}

fn classify_value(node: &Node) -> SymbolKind {
    match &*node.expr {
        Expr::Closure { .. } => SymbolKind::Method,
        _ => SymbolKind::Field,
    }
}

fn combine_pair_range(a: TokenRange, b: TokenRange) -> TokenRange {
    let start = if a.start.offset <= b.start.offset {
        a.start
    } else {
        b.start
    };
    let end = if a.end.offset >= b.end.offset {
        a.end
    } else {
        b.end
    };
    TokenRange { start, end }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    #[test]
    fn lists_top_level_fields_and_methods() {
        let src = r#"{
                name: "Relon",
                add: (x, y) => x + y,
                meta: { version: 1 }
            }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let syms = collect(&root, &tree);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"name"));
        assert!(names.contains(&"add"));
        assert!(names.contains(&"meta"));
        assert!(names.contains(&"version"));
        let add = syms.iter().find(|s| s.name == "add").unwrap();
        assert_eq!(add.kind, SymbolKind::Method);
    }

    #[test]
    fn schema_emits_kind_schema_with_fields_as_children() {
        let src = r#"{
                #schema User { String name: *, Int age: * },
                user: { name: "x", age: 1 }
            }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let syms = collect(&root, &tree);
        let user_schema = syms
            .iter()
            .enumerate()
            .find(|(_, s)| s.kind == SymbolKind::Schema && s.name == "User")
            .map(|(i, _)| i)
            .expect("schema present");
        let children: Vec<&DocumentSymbol> = syms
            .iter()
            .filter(|s| s.parent == Some(user_schema))
            .collect();
        let cnames: Vec<&str> = children.iter().map(|s| s.name.as_str()).collect();
        assert!(cnames.contains(&"name"), "{cnames:?}");
        assert!(cnames.contains(&"age"), "{cnames:?}");
        for c in &children {
            assert_eq!(c.kind, SymbolKind::SchemaField);
        }
    }

    #[test]
    fn nested_dict_fields_carry_parent_index() {
        let src = r#"{ project: { name: "x", details: { base: 1 } } }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let syms = collect(&root, &tree);
        let project = syms.iter().position(|s| s.name == "project").unwrap();
        let details = syms.iter().position(|s| s.name == "details").unwrap();
        let base = syms.iter().position(|s| s.name == "base").unwrap();
        assert_eq!(syms[details].parent, Some(project));
        assert_eq!(syms[base].parent, Some(details));
    }

    #[test]
    fn empty_dict_yields_empty_list() {
        let src = "{}";
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let syms = collect(&root, &tree);
        assert!(syms.is_empty(), "{syms:?}");
    }
}
