//! Find-references at a cursor position.
//!
//! Reverse-lookup over [`AnalyzedTree::references`]. The analyzer's
//! forward table maps every statically resolvable reference site
//! (`&sibling.X`, `&root.X`, `&uncle.X`, bare `Variable(X)`) to the
//! `NodeId` of the dict-field value it binds to. To answer
//! "find references" we walk the table the other way:
//!
//! 1. Locate the target `NodeId` the user is pointing at.
//!    * If the cursor sits on a reference/variable expression, we
//!      forward-resolve it and take its `target`.
//!    * If the cursor sits inside a dict-field key, we take the paired
//!      value's `NodeId`.
//!    * Otherwise the cursor is presumed to be on the definition value
//!      itself, so we use the smallest covering node's id.
//! 2. Collect every entry in `tree.references` whose `target` equals
//!    that id and turn each entry's `source_range` into a [`TokenRange`].
//! 3. When the caller asks for `include_declaration`, prepend the
//!    target's own range.
//!
//! Scope is in-file only. The forward reference table is module-local;
//! cross-module find-references would need a workspace-wide index that
//! `AnalyzedTree` doesn't carry yet.

use crate::goto_def::{covers, position_to_offset, smallest_node_at};
use crate::tree::AnalyzedTree;
use relon_parser::{Expr, Node, NodeId, TokenKey, TokenRange};

/// Resolve the references list at `(line, character)`. Returns
/// `None` only when the cursor sits on something we can't relate to
/// any node at all — when the target is known but has zero references,
/// returns `Some(vec![])`.
pub fn resolve(
    source: &str,
    root: &Node,
    tree: &AnalyzedTree,
    line: u32,
    character: u32,
    include_declaration: bool,
) -> Option<Vec<TokenRange>> {
    let offset = position_to_offset(source, line, character);
    let node = smallest_node_at(root, offset)?;

    let target_id = match &*node.expr {
        Expr::Reference { .. } | Expr::Variable(_) => tree.references.get(&node.id)?.target,
        _ => key_target_at(node, offset).unwrap_or(node.id),
    };

    let mut ranges = collect_references(tree, target_id);
    if include_declaration {
        if let Some(decl) = tree.node_index.get(&target_id) {
            ranges.insert(0, decl.range);
        }
    }
    Some(ranges)
}

fn key_target_at(node: &Node, offset: usize) -> Option<NodeId> {
    let Expr::Dict(pairs) = &*node.expr else {
        return None;
    };
    for (key, value) in pairs {
        if let TokenKey::String(_, range, _) = key {
            if covers(*range, offset) {
                return Some(value.id);
            }
        }
    }
    None
}

fn collect_references(tree: &AnalyzedTree, target_id: NodeId) -> Vec<TokenRange> {
    let mut out: Vec<TokenRange> = tree
        .references
        .values()
        .filter(|resolved| resolved.target == target_id)
        .map(|resolved| resolved.source_range)
        .collect();
    out.sort_by_key(|r| (r.start.line, r.start.column));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze;
    use relon_parser::parse_document;

    fn pos_at(src: &str, offset: usize) -> (u32, u32) {
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
                col += ch.len_utf16() as u32;
            }
        }
        (line, col)
    }

    #[test]
    fn finds_all_sibling_references_to_a_field() {
        let src = r#"{
                a: 10,
                b: &sibling.a,
                c: &sibling.a,
                d: &sibling.a
            }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let offset = src.find("a: 10").unwrap();
        let (line, character) = pos_at(src, offset);
        let locs = resolve(src, &root, &tree, line, character, false).expect("resolve");
        assert_eq!(locs.len(), 3, "{locs:?}");
    }

    #[test]
    fn finds_references_from_reference_site() {
        let src = r#"{
                a: 10,
                b: &sibling.a,
                c: &sibling.a,
                d: &sibling.a
            }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let offset = src.find("&sibling.a").unwrap() + "&sibling.".len();
        let (line, character) = pos_at(src, offset);
        let locs = resolve(src, &root, &tree, line, character, false).expect("resolve");
        assert_eq!(locs.len(), 3, "all three sibling refs: {locs:?}");
    }

    #[test]
    fn include_declaration_prepends_target_range() {
        let src = r#"{ a: 10, b: &sibling.a }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let offset = src.find("a: 10").unwrap();
        let (line, character) = pos_at(src, offset);
        let with = resolve(src, &root, &tree, line, character, true).expect("resolve");
        let without = resolve(src, &root, &tree, line, character, false).expect("resolve");
        assert_eq!(with.len(), without.len() + 1);
    }

    #[test]
    fn returns_empty_for_field_with_no_refs() {
        let src = r#"{ lonely: 1 }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let offset = src.find("lonely").unwrap();
        let (line, character) = pos_at(src, offset);
        let locs = resolve(src, &root, &tree, line, character, false).expect("resolve");
        assert!(locs.is_empty(), "{locs:?}");
    }
}
