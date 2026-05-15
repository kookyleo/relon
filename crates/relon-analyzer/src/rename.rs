//! Rename a dict-field symbol across an in-file scope.
//!
//! Builds on [`crate::references`]: locate the target node the cursor
//! is on, reverse-lookup every reference site, then emit one `TextEdit`
//! per occurrence — including the *key* of the declaring dict-pair —
//! to swap the old name for `new_name`.
//!
//! The rename is module-local. Cross-module references (the analyzer's
//! `WorkspaceTree` doesn't index those yet) are intentionally out of
//! scope; a future workspace-wide rename would batch results across
//! the per-module edit lists this returns.

use crate::goto_def::{covers, position_to_offset, smallest_node_at};
use crate::tree::AnalyzedTree;
use relon_parser::{Expr, Node, NodeId, TokenKey, TokenRange};

/// One text replacement. The caller wraps it in whatever editor /
/// LSP shape it needs (CodeMirror Transaction, `WorkspaceEdit`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: TokenRange,
    pub new_text: String,
}

/// Reasons a rename request can be refused. Surfaced to the caller so
/// IDEs can show a sensible "rename failed" toast instead of silently
/// dropping the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    /// Cursor wasn't on a recognisable symbol.
    NoSymbolAtCursor,
    /// `new_name` is empty, starts with a digit, or contains characters
    /// the lexer doesn't accept in an identifier.
    InvalidIdentifier(String),
    /// Cursor was on a reference site that doesn't resolve through
    /// the analyzer's forward table.
    UnresolvedReference,
    /// The target exists but the analyzer can't pin down a declaration
    /// range to edit — e.g. a synthetic node without a source span.
    NoDeclarationRange,
}

/// Compute the source range that *would* be replaced if the user
/// triggered a rename at `(line, character)`. IDEs use this to seed
/// the rename input box and to fail fast when the cursor isn't on
/// something renamable.
pub fn prepare(
    source: &str,
    root: &Node,
    tree: &AnalyzedTree,
    line: u32,
    character: u32,
) -> Result<TokenRange, RenameError> {
    let offset = position_to_offset(source, line, character);
    let node = smallest_node_at(root, offset).ok_or(RenameError::NoSymbolAtCursor)?;
    let (_target_id, decl_range) = resolve_target(tree, node, offset)?;
    Ok(decl_range)
}

/// Compute the full edit list to rename the symbol at `(line, character)`
/// to `new_name`. The returned `Vec` covers (declaration key) +
/// (every in-file reference site).
pub fn execute(
    source: &str,
    root: &Node,
    tree: &AnalyzedTree,
    line: u32,
    character: u32,
    new_name: &str,
) -> Result<Vec<TextEdit>, RenameError> {
    validate_identifier(new_name)?;
    let offset = position_to_offset(source, line, character);
    let node = smallest_node_at(root, offset).ok_or(RenameError::NoSymbolAtCursor)?;
    let (target_id, decl_range) = resolve_target(tree, node, offset)?;

    let mut edits = vec![TextEdit {
        range: decl_range,
        new_text: new_name.to_string(),
    }];

    for resolved in tree.references.values() {
        if resolved.target == target_id {
            // The reference's `source_range` covers the whole token —
            // `&sibling.foo` or `foo` — but we only want to replace
            // the trailing identifier. Probe the source slice to find
            // the last identifier-shaped run.
            edits.push(TextEdit {
                range: ident_tail_range(source, resolved.source_range),
                new_text: new_name.to_string(),
            });
        }
    }

    edits.sort_by_key(|e| e.range.start.offset);
    Ok(edits)
}

/// Resolve `node` to the symbol's `(target_id, declaration_key_range)`.
/// Mirrors the logic in [`crate::references::resolve`] but additionally
/// requires that we know the *key* range of the declaration so the
/// rename only rewrites the identifier itself, never the value.
fn resolve_target(
    tree: &AnalyzedTree,
    node: &Node,
    offset: usize,
) -> Result<(NodeId, TokenRange), RenameError> {
    let target_id = match &*node.expr {
        Expr::Reference { .. } | Expr::Variable(_) => {
            tree.references
                .get(&node.id)
                .ok_or(RenameError::UnresolvedReference)?
                .target
        }
        _ => key_target_at(node, offset).unwrap_or(node.id),
    };
    let decl_range =
        declaration_key_range(tree, target_id).ok_or(RenameError::NoDeclarationRange)?;
    Ok((target_id, decl_range))
}

/// Walk every indexed node looking for a dict whose pair-value matches
/// `target_id`. Returns the key's `TokenRange` so the rename rewrites
/// the identifier rather than the value.
fn declaration_key_range(tree: &AnalyzedTree, target_id: NodeId) -> Option<TokenRange> {
    for node in tree.node_index.values() {
        if let Expr::Dict(pairs) = &*node.expr {
            for (key, value) in pairs {
                if value.id == target_id {
                    if let TokenKey::String(_, range, _) = key {
                        return Some(*range);
                    }
                }
            }
        }
    }
    None
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

/// The reference `source_range` spans the full reference token (e.g.
/// `&sibling.foo`). We only want to replace the final identifier
/// — the chars matching `[A-Za-z_][A-Za-z0-9_]*` at the tail. This
/// keeps `&sibling`, `&root.`, etc. untouched.
fn ident_tail_range(source: &str, range: TokenRange) -> TokenRange {
    let end = range.end.offset.min(source.len());
    let start = range.start.offset.min(end);
    let bytes = source.as_bytes();
    let mut tail_start = end;
    while tail_start > start {
        let b = bytes[tail_start - 1];
        if b == b'_' || b.is_ascii_alphanumeric() {
            tail_start -= 1;
        } else {
            break;
        }
    }
    if tail_start == end {
        return range;
    }
    // Reconstruct a `TokenRange` for the trimmed slice. We recompute
    // `(line, column)` by counting newlines from the original start —
    // cheaper than re-lexing and accurate enough for in-file edits.
    let mut line = range.start.line;
    let mut col = range.start.column;
    for (i, ch) in source.char_indices().take_while(|(i, _)| *i < tail_start) {
        if i < range.start.offset {
            continue;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    let mut new = range;
    new.start.offset = tail_start;
    new.start.line = line;
    new.start.column = col;
    new
}

fn validate_identifier(name: &str) -> Result<(), RenameError> {
    if name.is_empty() {
        return Err(RenameError::InvalidIdentifier(
            "rename target may not be empty".into(),
        ));
    }
    let bytes = name.as_bytes();
    let first = bytes[0];
    if !(first == b'_' || first.is_ascii_alphabetic()) {
        return Err(RenameError::InvalidIdentifier(format!(
            "identifier must start with letter or underscore, got `{}`",
            name.chars().next().unwrap_or('?')
        )));
    }
    for (i, b) in bytes.iter().enumerate().skip(1) {
        if !(*b == b'_' || b.is_ascii_alphanumeric()) {
            return Err(RenameError::InvalidIdentifier(format!(
                "invalid character at position {i} in `{name}`"
            )));
        }
    }
    Ok(())
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

    fn apply_edits(src: &str, mut edits: Vec<TextEdit>) -> String {
        edits.sort_by_key(|e| std::cmp::Reverse(e.range.start.offset));
        let mut out = src.to_string();
        for e in edits {
            out.replace_range(e.range.start.offset..e.range.end.offset, &e.new_text);
        }
        out
    }

    #[test]
    fn renames_field_and_all_references() {
        let src = r#"{
                a: 10,
                b: &sibling.a,
                c: &sibling.a
            }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let off = src.find("a: 10").unwrap();
        let (line, character) = pos_at(src, off);
        let edits = execute(src, &root, &tree, line, character, "renamed").expect("rename");
        assert_eq!(edits.len(), 3, "{edits:?}");
        let updated = apply_edits(src, edits);
        assert!(updated.contains("renamed: 10"), "{updated}");
        assert!(updated.contains("&sibling.renamed"), "{updated}");
        assert!(!updated.contains("&sibling.a"), "{updated}");
    }

    #[test]
    fn prepare_returns_key_range() {
        let src = r#"{ a: 10, b: &sibling.a }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let off = src.find("a: 10").unwrap();
        let (line, character) = pos_at(src, off);
        let range = prepare(src, &root, &tree, line, character).expect("prepare");
        assert_eq!(range.start.offset, off);
        assert_eq!(range.end.offset, off + 1);
    }

    #[test]
    fn rejects_invalid_identifier() {
        let src = r#"{ a: 10 }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let off = src.find("a: 10").unwrap();
        let (line, character) = pos_at(src, off);
        let err = execute(src, &root, &tree, line, character, "1bad")
            .expect_err("should reject digit-leading");
        assert!(matches!(err, RenameError::InvalidIdentifier(_)));
    }

    #[test]
    fn no_symbol_at_cursor_yields_error() {
        let src = r#"{ a: 10 }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        let err = execute(src, &root, &tree, 99, 0, "x").expect_err("out-of-bounds cursor");
        // Either NoSymbolAtCursor or NoDeclarationRange depending on
        // clamping behaviour; both are sensible failure modes.
        assert!(
            matches!(
                err,
                RenameError::NoSymbolAtCursor
                    | RenameError::NoDeclarationRange
                    | RenameError::UnresolvedReference
            ),
            "got: {err:?}"
        );
    }

    #[test]
    fn rename_from_reference_site_still_targets_declaration() {
        let src = r#"{ a: 10, b: &sibling.a }"#;
        let root = parse_document(src).unwrap();
        let tree = analyze(&root);
        // Cursor on the `a` inside `&sibling.a`.
        let off = src.find("&sibling.a").unwrap() + "&sibling.".len();
        let (line, character) = pos_at(src, off);
        let edits = execute(src, &root, &tree, line, character, "renamed").expect("rename");
        let updated = apply_edits(src, edits);
        assert!(updated.contains("renamed: 10"), "{updated}");
        assert!(updated.contains("&sibling.renamed"), "{updated}");
    }
}
