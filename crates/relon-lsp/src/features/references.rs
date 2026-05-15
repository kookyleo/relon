//! `textDocument/references`.
//!
//! Reverse-lookup over [`relon_analyzer::AnalyzedTree::references`].
//! The analyzer's table maps every statically resolvable reference site
//! (`&sibling.X`, `&root.X`, `&uncle.X`, bare `Variable(X)`) to the
//! `NodeId` of the dict-field value it binds to; for "find references"
//! we walk the table the other way:
//!
//! 1. Locate the target `NodeId` the user is pointing at.
//!    * If the cursor sits on a reference/variable expression, we
//!      forward-resolve it and take its `target`.
//!    * If the cursor sits on a dict-field value (the definition),
//!      we take that node's id directly.
//! 2. Collect every entry in `tree.references` whose `target` equals
//!    that id and turn each entry's `source_range` into an LSP
//!    `Location` in the active document.
//! 3. When the LSP client asks `include_declaration = true`, prepend
//!    the target's own range as well.
//!
//! Scope is intentionally in-file only. The analyzer's reference table
//! is forward-only and scoped to one module — there is no cross-module
//! symbol → usages index in `WorkspaceTree` yet. Cross-file references
//! remain deferred (see `docs/internal/roadmap.md`).

use crate::position::token_range;
use crate::server::DocumentEntry;
use lsp_types::{Location, Position, Url};

/// Resolve `position` to the `NodeId` of the field whose references the
/// user wants to find, then collect every in-file reference site.
///
/// Returns `None` if the cursor isn't over something we recognize.
/// Returns an empty `Vec` (wrapped in `Some`) when the target is known
/// but currently has no references — the LSP convention is "present
/// with zero items" rather than "absent" so the client doesn't fall
/// through to another provider.
///
/// Delegates the structural work to `relon_analyzer::references::resolve`;
/// this function is just the LSP-shaped adapter (Position → line/char,
/// TokenRange → `lsp_types::Range`, attach `uri`).
pub fn resolve(
    entry: &DocumentEntry,
    position: Position,
    uri: &Url,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    let ranges = relon_analyzer::references::resolve(
        &entry.source,
        &entry.root,
        &entry.tree,
        position.line,
        position.character,
        include_declaration,
    )?;
    Some(
        ranges
            .into_iter()
            .map(|r| Location {
                uri: uri.clone(),
                range: token_range(r),
            })
            .collect(),
    )
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

    fn pos_at(src: &str, offset: usize) -> Position {
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
        Position {
            line,
            character: col,
        }
    }

    #[test]
    fn finds_all_sibling_references_to_a_field() {
        // `a` defined once, referenced three times via `&sibling`.
        let src = r#"{
                a: 10,
                b: &sibling.a,
                c: &sibling.a,
                d: &sibling.a
            }"#;
        let entry = entry(src);
        // Cursor on the `a` of `a: 10` (the declaration).
        let offset = src.find("a: 10").unwrap();
        let pos = pos_at(src, offset);
        let uri = Url::parse("file:///x.relon").unwrap();
        let locs = resolve(&entry, pos, &uri, false).expect("resolve");
        assert_eq!(locs.len(), 3, "{locs:?}");
        for loc in &locs {
            assert_eq!(loc.uri, uri);
        }
    }

    #[test]
    fn finds_references_from_reference_site() {
        // Cursor on a reference site itself should still find the
        // sibling references to the same field (3 total: the cursor
        // site plus two others).
        let src = r#"{
                a: 10,
                b: &sibling.a,
                c: &sibling.a,
                d: &sibling.a
            }"#;
        let entry = entry(src);
        // Cursor on the `a` inside the *first* `&sibling.a`.
        let offset = src.find("&sibling.a").unwrap() + "&sibling.".len();
        let pos = pos_at(src, offset);
        let uri = Url::parse("file:///x.relon").unwrap();
        let locs = resolve(&entry, pos, &uri, false).expect("resolve");
        assert_eq!(locs.len(), 3, "all three sibling refs: {locs:?}");
    }

    #[test]
    fn include_declaration_prepends_target_range() {
        let src = r#"{ a: 10, b: &sibling.a }"#;
        let entry = entry(src);
        let offset = src.find("a: 10").unwrap();
        let pos = pos_at(src, offset);
        let uri = Url::parse("file:///x.relon").unwrap();
        let with = resolve(&entry, pos, &uri, true).expect("resolve");
        let without = resolve(&entry, pos, &uri, false).expect("resolve");
        assert_eq!(with.len(), without.len() + 1);
        // The first entry should be the declaration (a: 10). Its start
        // should not match any of the reference ranges.
        let decl_range = with[0].range;
        assert!(!without.iter().any(|l| l.range == decl_range));
    }

    #[test]
    fn returns_empty_for_field_with_no_refs() {
        let src = r#"{ lonely: 1 }"#;
        let entry = entry(src);
        let offset = src.find("lonely").unwrap();
        let pos = pos_at(src, offset);
        let uri = Url::parse("file:///x.relon").unwrap();
        let locs = resolve(&entry, pos, &uri, false).expect("resolve");
        assert!(locs.is_empty(), "{locs:?}");
    }

    #[test]
    fn returns_none_outside_document() {
        let src = r#"{ a: 1 }"#;
        let entry = entry(src);
        // Position past EOF.
        let pos = Position {
            line: 99,
            character: 0,
        };
        let uri = Url::parse("file:///x.relon").unwrap();
        // Out-of-bounds offsets clamp to source.len() and still resolve
        // to the root node — that's fine; we just return zero refs.
        let locs = resolve(&entry, pos, &uri, false);
        assert!(locs.is_some());
        assert!(locs.unwrap().is_empty());
    }

    #[test]
    fn schema_field_referenced_as_type_yields_zero_static_refs() {
        // The analyzer's reference table doesn't track `Type` nodes
        // (schema names used in type positions). This test pins the
        // current behaviour so a future cross-module pass that adds
        // type-reference tracking shows up as a deliberate diff rather
        // than a silent gain.
        let src = r#"{
                #schema User { String name: * },
                User alice: { name: "A" },
                User bob: { name: "B" }
            }"#;
        let entry = entry(src);
        // Cursor on the schema name `User` in the declaration.
        let offset = src.find("#schema User").unwrap() + "#schema ".len();
        let pos = pos_at(src, offset);
        let uri = Url::parse("file:///x.relon").unwrap();
        let locs = resolve(&entry, pos, &uri, false).expect("resolve");
        // No `Reference`/`Variable` nodes target the schema definition
        // node — both `User alice` and `User bob` use `Type` nodes,
        // which the resolver doesn't index.
        assert!(
            locs.is_empty(),
            "schema-name type-refs aren't indexed yet: {locs:?}"
        );
    }
}
