//! `textDocument/completion`.
//!
//! Two complementary sources:
//!
//! 1. **Sibling field names** — when the cursor sits inside a dict
//!    body, suggest the names of its already-declared keys (useful
//!    for typing references like `&sibling.X`).
//! 2. **Schema names** — every `@schema` declaration in the document
//!    contributes a suggestion. Helpful when typing a typed field
//!    binding such as `User alice: { ... }`.
//!
//! No fancy ranking, no filtering by prefix — the LSP client does
//! the prefix match. We just offer the candidates.

use crate::features::cursor::smallest_node_at;
use crate::position::position_to_offset;
use crate::server::DocumentEntry;
use lsp_types::{CompletionItem, CompletionItemKind, Position};
use relon_parser::{child_nodes, Expr, Node, TokenKey};
use std::collections::BTreeSet;


pub fn items_for(entry: &DocumentEntry, position: Position) -> Vec<CompletionItem> {
    let mut out = BTreeSet::new();

    // Sibling fields from whichever dict encloses the cursor.
    let offset = position_to_offset(&entry.source, position);
    if let Some(node) = smallest_node_at(&entry.root, offset) {
        if let Some(dict_pairs) = enclosing_dict(&entry.root, node) {
            for (key, _) in dict_pairs {
                if let TokenKey::String(s, _, _) = key {
                    out.insert(SuggestionKey::Field(s.clone()));
                }
            }
        }
    }

    // Every schema name in the document.
    for def in entry.tree.schemas.values() {
        if let Some(name) = &def.name {
            out.insert(SuggestionKey::Schema(name.clone()));
        }
    }

    out.into_iter().map(into_item).collect()
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SuggestionKey {
    Schema(String),
    Field(String),
}

fn into_item(key: SuggestionKey) -> CompletionItem {
    match key {
        SuggestionKey::Field(label) => CompletionItem {
            label,
            kind: Some(CompletionItemKind::FIELD),
            ..Default::default()
        },
        SuggestionKey::Schema(label) => CompletionItem {
            label,
            kind: Some(CompletionItemKind::CLASS),
            detail: Some("schema".to_string()),
            ..Default::default()
        },
    }
}

/// Find the smallest dict whose body encloses `inner`. Returns the
/// pairs slice on success. We use this rather than `smallest_node_at`
/// directly because the cursor often lands on a value node *inside*
/// the dict, not the dict itself.
fn enclosing_dict<'a>(root: &'a Node, inner: &'a Node) -> Option<&'a [(TokenKey, Node)]> {
    let mut found: Option<&[(TokenKey, Node)]> = None;
    walk(root, inner, &mut found);
    found
}

fn walk<'a>(node: &'a Node, target: &'a Node, found: &mut Option<&'a [(TokenKey, Node)]>) {
    if !contains(node, target) {
        return;
    }
    if let Expr::Dict(pairs) = &*node.expr {
        // Only update if this dict is strictly tighter than the
        // current candidate — otherwise we'd promote to the outermost
        // dict and lose the local context.
        if found
            .as_ref()
            .map(|p| dict_size(node) < pairs_size(p))
            .unwrap_or(true)
        {
            *found = Some(pairs.as_slice());
        }
    }
    for child in child_nodes(node) {
        walk(child, target, found);
    }
}

fn contains(node: &Node, inner: &Node) -> bool {
    node.range.start.offset <= inner.range.start.offset
        && node.range.end.offset >= inner.range.end.offset
}

fn dict_size(node: &Node) -> usize {
    node.range
        .end
        .offset
        .saturating_sub(node.range.start.offset)
}

fn pairs_size(pairs: &[(TokenKey, Node)]) -> usize {
    pairs
        .iter()
        .map(|(_, v)| v.range.end.offset.saturating_sub(v.range.start.offset))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::DocumentEntry;
    use relon_analyzer::analyze;
    use relon_parser::parse_document;
    use std::sync::Arc;

    fn entry(src: &str) -> DocumentEntry {
        let root = Arc::new(parse_document(src).unwrap());
        let tree = Arc::new(analyze(&root));
        DocumentEntry {
            source: src.to_string(),
            root,
            tree,
        }
    }

    #[test]
    fn suggests_sibling_fields_and_schema_names() {
        let src = r#"{
                @schema User: { String name: * },
                a: 1,
                b: 2
            }"#;
        let entry = entry(src);
        // Cursor at column 0 on the second-to-last line — sits inside
        // the root dict so all root keys + the schema name should
        // appear.
        let items = items_for(
            &entry,
            Position {
                line: 3,
                character: 0,
            },
        );
        let labels: BTreeSet<String> = items.into_iter().map(|i| i.label).collect();
        assert!(labels.contains("User"), "{labels:?}");
        assert!(labels.contains("a"), "{labels:?}");
        assert!(labels.contains("b"), "{labels:?}");
    }

    #[test]
    fn schema_only_when_no_dict_context() {
        // Cursor before any dict (line 0, character 0). The root dict
        // still encloses it, but the test mostly confirms we don't
        // panic and the schema names always make it through.
        let src = r#"{ @schema A: { String x: * } }"#;
        let entry = entry(src);
        let items = items_for(
            &entry,
            Position {
                line: 0,
                character: 0,
            },
        );
        let labels: BTreeSet<String> = items.into_iter().map(|i| i.label).collect();
        assert!(labels.contains("A"), "{labels:?}");
    }
}
