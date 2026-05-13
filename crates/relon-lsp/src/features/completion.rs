//! `textDocument/completion`.
//!
//! Thin adapter over [`relon_analyzer::complete::resolve`]. The
//! analyzer is the source of truth for scope, ownership, and
//! cursor-context classification; we only translate the result to
//! `lsp_types::CompletionItem`.

use crate::server::DocumentEntry;
use lsp_types::{CompletionItem, CompletionItemKind, Position};
use relon_analyzer::complete::{self, CompletionKind};

pub fn items_for(entry: &DocumentEntry, position: Position) -> Vec<CompletionItem> {
    complete::resolve(
        &entry.source,
        &entry.root,
        &entry.tree,
        None,
        position.line,
        position.character,
    )
    .into_iter()
    .map(into_item)
    .collect()
}

fn into_item(item: complete::CompletionItem) -> CompletionItem {
    CompletionItem {
        label: item.label,
        kind: Some(lsp_kind(item.kind)),
        detail: item.detail,
        ..Default::default()
    }
}

fn lsp_kind(k: CompletionKind) -> CompletionItemKind {
    match k {
        CompletionKind::Method => CompletionItemKind::METHOD,
        CompletionKind::Field => CompletionItemKind::FIELD,
        CompletionKind::Parameter => CompletionItemKind::VARIABLE,
        CompletionKind::Schema => CompletionItemKind::CLASS,
        CompletionKind::Stdlib => CompletionItemKind::FUNCTION,
        CompletionKind::Module => CompletionItemKind::MODULE,
        CompletionKind::Import => CompletionItemKind::FUNCTION,
        CompletionKind::Reference => CompletionItemKind::VARIABLE,
        CompletionKind::Directive | CompletionKind::Pragma => CompletionItemKind::KEYWORD,
        CompletionKind::Decorator => CompletionItemKind::FUNCTION,
        CompletionKind::Keyword => CompletionItemKind::KEYWORD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::DocumentEntry;
    use relon_analyzer::analyze;
    use relon_parser::parse_document;
    use std::collections::BTreeSet;
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
        let src = "#schema User { String name: * }\n\n{\n    a: 1,\n    b: 2\n}\n";
        let entry = entry(src);
        // Cursor inside the root dict's `1` value.
        let items = items_for(
            &entry,
            Position {
                line: 3,
                character: 7,
            },
        );
        let labels: BTreeSet<String> = items.into_iter().map(|i| i.label).collect();
        assert!(labels.contains("User"), "{labels:?}");
        assert!(labels.contains("a"), "{labels:?}");
        assert!(labels.contains("b"), "{labels:?}");
    }
}
