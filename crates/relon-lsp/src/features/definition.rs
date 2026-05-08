//! `textDocument/definition`.
//!
//! Walk the cursor to a `Reference` / `Variable` node, look it up in
//! `AnalyzedTree.references`, and return the target value-node's
//! source range as a `Location` in the *same* document URI.
//!
//! Cross-file definitions (jumping into an `#import`ed module) need
//! the analyzer's module graph to track URIs per module, which it
//! doesn't yet — so this handler always returns the active document's
//! URI. Out-of-document targets simply don't resolve.

use crate::features::cursor::{covers, smallest_node_at};
use crate::position::{position_to_offset, token_range};
use crate::server::DocumentEntry;
use lsp_types::{Location, Position, Range, Url};
use relon_parser::Expr;

/// Resolve `position` (in `entry`) to the location of the field a
/// reference at the cursor points at. The returned `Location` always
/// carries `uri`; callers don't have to overwrite it.
///
/// Three cases are checked, in order:
///
/// 1. Cursor on an `#import path from "path"` path string — jump to the start
///    of the imported file (URI computed relative to `uri`).
/// 2. Cursor on a `Reference { ... }` / `Variable(...)` whose
///    analyzer binding hits — return the target field's range.
/// 3. Otherwise — `None`.
pub fn resolve(entry: &DocumentEntry, position: Position, uri: &Url) -> Option<Location> {
    let offset = position_to_offset(&entry.source, position);

    if let Some(loc) = import_target(entry, offset, uri) {
        return Some(loc);
    }

    let node = smallest_node_at(&entry.root, offset)?;
    match &*node.expr {
        Expr::Reference { .. } | Expr::Variable(_) => {}
        _ => return None,
    }
    let resolved = entry.tree.references.get(&node.id)?;
    let target = entry.tree.node_index.get(&resolved.target)?;
    Some(Location {
        uri: uri.clone(),
        range: token_range(target.range),
    })
}

/// If `offset` falls inside any `@import(path, ...)` decorator's
/// `path` string, return a `Location` pointing at the start of the
/// resolved file. Returns `None` for non-string paths (dynamic
/// f-strings) or unresolvable URIs (non-`file://` schemes, paths that
/// don't exist on disk are still returned — LSP clients treat that as
/// a "create file" affordance).
fn import_target(entry: &DocumentEntry, offset: usize, uri: &Url) -> Option<Location> {
    for import in &entry.tree.imports {
        if !covers(import.range, offset) {
            continue;
        }
        let path = import.path.as_deref()?;
        let target_uri = resolve_import_uri(uri, path)?;
        return Some(Location {
            uri: target_uri,
            range: Range::default(),
        });
    }
    None
}

/// Translate an import path, relative to the importing document's
/// URI, into the target document's URI. We deliberately don't touch
/// the filesystem here — the LSP spec lets us return URIs that point
/// at unopened (or even non-existent) files; the client decides what
/// to do.
///
/// `std/...` virtual-module paths are skipped (no source file to
/// jump to).
fn resolve_import_uri(base: &Url, path: &str) -> Option<Url> {
    if path.starts_with("std/") {
        return None;
    }
    base.join(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::DocumentEntry;
    use lsp_types::Url;
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
    fn resolves_sibling_reference_to_target_field() {
        let src = "{ a: 10, b: &sibling.a }";
        let entry = entry(src);
        // Cursor on the `a` inside `&sibling.a` — column 22.
        let pos = Position {
            line: 0,
            character: 22,
        };
        let uri = Url::parse("file:///x.relon").unwrap();
        let loc = resolve(&entry, pos, &uri).expect("must resolve");
        assert_eq!(loc.uri, uri);
        // Target should sit on line 0, somewhere over the literal `10`.
        assert_eq!(loc.range.start.line, 0);
    }

    #[test]
    fn returns_none_off_reference() {
        let src = "{ a: 10 }";
        let entry = entry(src);
        // Cursor on the `1` of `10` — that's a literal, not a ref.
        let pos = Position {
            line: 0,
            character: 5,
        };
        let uri = Url::parse("file:///x.relon").unwrap();
        assert!(resolve(&entry, pos, &uri).is_none());
    }

    #[test]
    fn jumps_from_import_path_to_target_file() {
        // Cursor on the `lib.relon` literal inside `#import lib from "lib.relon"`.
        let src = r#"#import lib from "lib.relon" { x: lib.x }"#;
        let entry = entry(src);
        // Find the offset of the first 'l' inside the import path.
        let offset = src.find("lib.relon").unwrap();
        let mut col = 0u32;
        for ch in src.chars().take(offset) {
            col += ch.len_utf16() as u32;
        }
        let pos = Position {
            line: 0,
            character: col,
        };
        let uri = Url::parse("file:///project/main.relon").unwrap();
        let loc = resolve(&entry, pos, &uri).expect("import jump");
        assert_eq!(
            loc.uri.as_str(),
            "file:///project/lib.relon",
            "wrong target URI"
        );
    }

    #[test]
    fn skips_std_module_imports() {
        let src = r#"#import list from "std/list" { ok: list.first([1]) }"#;
        let entry = entry(src);
        let offset = src.find("std/list").unwrap();
        let mut col = 0u32;
        for ch in src.chars().take(offset) {
            col += ch.len_utf16() as u32;
        }
        let pos = Position {
            line: 0,
            character: col,
        };
        let uri = Url::parse("file:///x.relon").unwrap();
        assert!(
            resolve(&entry, pos, &uri).is_none(),
            "std/ paths shouldn't produce a file Location"
        );
    }
}
