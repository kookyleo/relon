//! Cursor-position hover information.
//!
//! Returns a Markdown-formatted string describing the symbol under the
//! cursor — the resolved target of a reference, or the typed signature
//! of a schema field — suitable for display in an IDE tooltip. The
//! `&str` return is intentionally opaque so the renderer (LSP / WASM)
//! decides how to wrap it.

use crate::tree::AnalyzedTree;
use crate::SchemaFieldDef;
use relon_parser::{Expr, Node, TokenRange, TypeNode};

/// Result of a successful hover lookup. `range` is the source span the
/// IDE should highlight to indicate which token the tooltip belongs to.
#[derive(Debug, Clone)]
pub struct HoverInfo {
    pub markdown: String,
    pub range: TokenRange,
}

/// Resolve a hover request at `(line, character)` (UTF-16 positions).
/// Returns `None` when the cursor sits on something we can't describe.
pub fn resolve(
    source: &str,
    root: &Node,
    tree: &AnalyzedTree,
    line: u32,
    character: u32,
) -> Option<HoverInfo> {
    let offset = crate::goto_def::position_to_offset(source, line, character);
    let node = crate::goto_def::smallest_node_at(root, offset)?;

    if let Some(md) = reference_hover(source, tree, node) {
        return Some(HoverInfo {
            markdown: md,
            range: node.range,
        });
    }
    if let Some(md) = schema_field_hover(tree, node) {
        return Some(HoverInfo {
            markdown: md,
            range: node.range,
        });
    }
    None
}

fn reference_hover(source: &str, tree: &AnalyzedTree, node: &Node) -> Option<String> {
    match &*node.expr {
        Expr::Reference { .. } | Expr::Variable(_) => {}
        _ => return None,
    }
    let resolved = tree.references.get(&node.id)?;
    let target = tree.node_index.get(&resolved.target)?;
    let snippet = source_slice(
        source,
        target.range.start.offset,
        target.range.end.offset,
    );
    let mut body = format!(
        "**Resolves to** _(via `{:?}`)_\n\n```relon\n{}\n```",
        resolved.via,
        snippet.trim_end()
    );
    if let Some(doc) = &target.doc_comment {
        body.push_str("\n\n---\n\n");
        body.push_str(doc);
    }
    Some(body)
}

fn schema_field_hover(tree: &AnalyzedTree, node: &Node) -> Option<String> {
    for def in tree.schemas.values() {
        if let Some(field) = def.fields.iter().find(|f| inside(f, node)) {
            let header = match &field.type_hint {
                Some(t) => format!(
                    "**`{} {}`** _(in schema `{}`)_",
                    format_type(t),
                    field.name,
                    def.name.as_deref().unwrap_or("?"),
                ),
                None => format!(
                    "**`{}`** _(in schema `{}` — untyped)_",
                    field.name,
                    def.name.as_deref().unwrap_or("?"),
                ),
            };
            let footer = if field.is_wildcard {
                "value: `*` (wildcard — accepts any matching value)"
            } else {
                "value: predicate / literal"
            };
            let mut body = format!("{header}\n\n{footer}");
            if let Some(doc) = &field.doc_comment {
                body.push_str("\n\n---\n\n");
                body.push_str(doc);
            }
            return Some(body);
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
