//! `textDocument/hover`.
//!
//! Two cases are handled:
//!
//! 1. Cursor on a `Reference` / `Variable` site that the analyzer
//!    bound — render the target's source snippet inside a Markdown
//!    code block, prefixed by the resolved field name.
//! 2. Cursor on a schema field's value position — render the static
//!    `Type field` signature plus an indication of whether the field
//!    has a wildcard or predicate body.
//!
//! Anything else returns `None` (no hover) so the client can fall
//! through to other servers.

use crate::features::cursor::smallest_node_at;
use crate::position::{position_to_offset, token_range};
use crate::server::DocumentEntry;
use lsp_types::{Hover, HoverContents, MarkupContent, MarkupKind, Position};
use relon_analyzer::SchemaFieldDef;
use relon_parser::{Expr, Node};

pub fn compute(entry: &DocumentEntry, position: Position) -> Option<Hover> {
    let offset = position_to_offset(&entry.source, position);
    let node = smallest_node_at(&entry.root, offset)?;

    if let Some(content) = hover_for_reference(entry, node) {
        return Some(Hover {
            contents: HoverContents::Markup(content),
            range: Some(token_range(node.range)),
        });
    }
    if let Some(content) = hover_for_schema_field(entry, node) {
        return Some(Hover {
            contents: HoverContents::Markup(content),
            range: Some(token_range(node.range)),
        });
    }
    None
}

fn hover_for_reference(entry: &DocumentEntry, node: &Node) -> Option<MarkupContent> {
    match &*node.expr {
        Expr::Reference { .. } | Expr::Variable(_) => {}
        _ => return None,
    }
    let resolved = entry.tree.references.get(&node.id)?;
    let target = entry.tree.node_index.get(&resolved.target)?;
    let snippet = source_slice(
        &entry.source,
        target.range.start.offset,
        target.range.end.offset,
    );
    let mut body = format!(
        "**Resolves to** _(via `{:?}`)_\n\n```relon\n{}\n```",
        resolved.via,
        snippet.trim_end()
    );
    if let Some(doc) = &target.doc_comment {
        body = format!("{}\n\n---\n\n{}", body, doc);
    }
    Some(MarkupContent {
        kind: MarkupKind::Markdown,
        value: body,
    })
}

fn hover_for_schema_field(entry: &DocumentEntry, node: &Node) -> Option<MarkupContent> {
    // Find a `SchemaDef` whose value-range contains this node.
    for def in entry.tree.schemas.values() {
        if let Some(field) = def.fields.iter().find(|f| inside(f, node)) {
            let header = match &field.type_hint {
                Some(t) => format!(
                    "**`{} {}`** _(in schema `{}`)_",
                    format_type(t),
                    field.name,
                    def.name.as_deref().unwrap_or("?")
                ),
                None => format!(
                    "**`{}`** _(in schema `{}` — untyped)_",
                    field.name,
                    def.name.as_deref().unwrap_or("?")
                ),
            };
            let footer = if field.is_wildcard {
                "value: `*` (wildcard — accepts any matching value)"
            } else {
                "value: predicate / literal"
            };
            let mut body = format!("{header}\n\n{footer}");
            if let Some(doc) = &field.doc_comment {
                body = format!("{}\n\n---\n\n{}", body, doc);
            }
            return Some(MarkupContent {
                kind: MarkupKind::Markdown,
                value: body,
            });
        }
    }
    None
}

fn inside(field: &SchemaFieldDef, node: &Node) -> bool {
    let r = field.value_range;
    let n = node.range;
    n.start.offset >= r.start.offset && n.end.offset <= r.end.offset
}

fn source_slice(source: &str, start: usize, end: usize) -> &str {
    let start = start.min(source.len());
    let end = end.min(source.len()).max(start);
    &source[start..end]
}

fn format_type(t: &relon_parser::TypeNode) -> String {
    let suffix = if t.is_optional { "?" } else { "" };
    let path = t.path.join(".");
    if t.generics.is_empty() {
        format!("{path}{suffix}")
    } else {
        let inner: Vec<String> = t.generics.iter().map(format_type).collect();
        format!("{path}<{}>{suffix}", inner.join(", "))
    }
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
    fn hovers_reference_with_target_snippet() {
        let entry = entry("{ a: 10, b: &sibling.a }");
        // Cursor on the `a` inside `&sibling.a`.
        let pos = Position {
            line: 0,
            character: 22,
        };
        let hover = compute(&entry, pos).expect("hover");
        let HoverContents::Markup(content) = hover.contents else {
            panic!()
        };
        assert!(content.value.contains("Resolves to"), "{}", content.value);
        assert!(content.value.contains("10"), "{}", content.value);
    }

    #[test]
    fn hovers_schema_field_with_type_signature() {
        let src = r#"{
                #schema User { String name: * },
                User u: { name: "x" }
            }"#;
        let entry = entry(src);
        // Cursor inside the schema field value — `*` after `String name:`.
        // Find offset of `*`.
        let offset = src.find('*').unwrap();
        // Convert to LSP position.
        let mut line = 0u32;
        let mut col = 0u32;
        for (i, ch) in src.chars().enumerate() {
            if i == offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        let hover = compute(
            &entry,
            Position {
                line,
                character: col,
            },
        )
        .expect("hover");
        let HoverContents::Markup(content) = hover.contents else {
            panic!()
        };
        assert!(content.value.contains("name"));
        assert!(content.value.contains("schema `User`"));
    }

    #[test]
    fn hovers_with_doc_comment() {
        let src = r#"{
                // The user schema.
                #schema User {
                  // The name of the person.
                  String name: *
                },
                // The primary user.
                User u: { name: "x" },
                ref: &sibling.u
            }"#;
        let entry = entry(src);

        // Hover over `u` in `&sibling.u`
        let offset = src.find("&sibling.u").unwrap() + 9;
        let pos = offset_to_pos(src, offset);
        let hover = compute(&entry, pos).expect("hover reference");
        let HoverContents::Markup(content) = hover.contents else {
            panic!()
        };
        // This is a reference hover (case 1)
        assert!(content.value.contains("The primary user."));

        // Hover over `*` in schema
        let offset = src.find('*').unwrap();
        let pos = offset_to_pos(src, offset);
        let hover = compute(&entry, pos).expect("hover schema field");
        let HoverContents::Markup(content) = hover.contents else {
            panic!()
        };
        assert!(content.value.contains("The name of the person."));
    }

    fn offset_to_pos(src: &str, offset: usize) -> Position {
        let mut line = 0u32;
        let mut col = 0u32;
        for (i, ch) in src.chars().enumerate() {
            if i == offset {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        Position {
            line,
            character: col,
        }
    }
}
