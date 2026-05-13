//! `textDocument/definition`.
//!
//! Walk the cursor to a `Reference` / `Variable` / `FnCall` node, look it
//! up in `AnalyzedTree.references` (same-file) or
//! `AnalyzedTree.cross_module_references` (resolved against a
//! `WorkspaceTree` when one is provided), and return the target
//! value-node's source range as a `Location`.
//!
//! When the caller passes `Some(workspace)`, cross-file jumps work
//! through `#import` bindings: alias-form (`lib.x`), destructure
//! (`#import { a }`) and spread (`#import *`). Without a workspace,
//! only same-file resolution runs — matching the pre-cross-file
//! behaviour for callers that haven't wired workspace state yet.

use crate::features::cursor::{covers, smallest_node_at};
use crate::position::{position_to_offset, token_range};
use crate::server::DocumentEntry;
use lsp_types::{Location, Position, Range, Url};
use relon_analyzer::WorkspaceTree;
use relon_parser::Expr;

/// Resolve `position` (in `entry`) to a definition location. The
/// returned `Location` carries `uri` for in-document hits and a
/// URI derived from the imported module's canonical id (a filesystem
/// path) for cross-file hits.
///
/// Cases checked, in order:
///
/// 1. Cursor on an `#import path from "path"` path string — jump to
///    the start of the imported file (URI computed relative to `uri`).
/// 2. Cursor on a node that has a [`relon_analyzer::CrossModuleRef`]
///    (when a `WorkspaceTree` is supplied) — jump cross-file.
/// 3. Cursor on a `Reference` / `Variable` / `FnCall` whose
///    analyzer binding hits — return the same-file target range.
/// 4. Otherwise — `None`.
pub fn resolve(
    entry: &DocumentEntry,
    position: Position,
    uri: &Url,
    workspace: Option<&WorkspaceTree>,
) -> Option<Location> {
    let offset = position_to_offset(&entry.source, position);

    if let Some(loc) = import_target(entry, offset, uri) {
        return Some(loc);
    }

    let node = smallest_node_at(&entry.root, offset)?;
    match &*node.expr {
        Expr::Reference { .. } | Expr::Variable(_) | Expr::FnCall { .. } => {}
        _ => return None,
    }
    // Cross-module first: a node that has a cross_module_references
    // entry is by construction not in `references` (we only queue the
    // pending entry after the in-document lookup misses).
    if let Some(ws) = workspace {
        if let Some(cross) = entry.tree.cross_module_references.get(&node.id) {
            return cross_module_location(ws, cross);
        }
    }
    let resolved = entry.tree.references.get(&node.id)?;
    let target = entry.tree.node_index.get(&resolved.target)?;
    Some(Location {
        uri: uri.clone(),
        range: token_range(target.range),
    })
}

/// Build a `Location` for a [`CrossModuleRef`]. When the target field
/// is known we point at its range; when it's not (alias head alone),
/// we land at offset 0 of the target file so the client at least
/// switches to the right document.
fn cross_module_location(
    workspace: &WorkspaceTree,
    cross: &relon_analyzer::CrossModuleRef,
) -> Option<Location> {
    let target_uri = module_id_to_uri(&cross.module_id)?;
    if let Some(target_id) = cross.target {
        let target_tree = workspace.modules.get(&cross.module_id)?;
        let target_node = target_tree.node_index.get(&target_id)?;
        return Some(Location {
            uri: target_uri,
            range: token_range(target_node.range),
        });
    }
    Some(Location {
        uri: target_uri,
        range: Range::default(),
    })
}

/// Translate a canonical module id back into an LSP URI. The workspace
/// pass keys modules by canonical filesystem paths (for disk-backed
/// modules) or synthetic ids (`std/...`, in-memory playground keys);
/// the former map cleanly via `Url::from_file_path`, the latter need
/// to be passed through as a `file:`-shaped path so clients have
/// *something* to navigate to. We try the filesystem form first and
/// fall back to a synthetic `relon-module://` scheme.
fn module_id_to_uri(canonical_id: &str) -> Option<Url> {
    if let Ok(uri) = Url::from_file_path(canonical_id) {
        return Some(uri);
    }
    Url::parse(&format!("relon-module:///{canonical_id}")).ok()
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
        let loc = resolve(&entry, pos, &uri, None).expect("must resolve");
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
        assert!(resolve(&entry, pos, &uri, None).is_none());
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
        let loc = resolve(&entry, pos, &uri, None).expect("import jump");
        assert_eq!(
            loc.uri.as_str(),
            "file:///project/lib.relon",
            "wrong target URI"
        );
    }

    /// Build an in-memory workspace + DocumentEntry pair so tests can
    /// drive the cross-file resolver without going through the LSP
    /// server's filesystem-backed pass. Returns `(entry, workspace)`
    /// — the workspace tree holds both modules' analyzed side-tables;
    /// the entry mirrors what the server's document store would hold.
    fn workspace_entry(
        entry_id: &str,
        entry_source: &str,
        loader_files: &[(&str, &str, &str)],
    ) -> (DocumentEntry, relon_analyzer::WorkspaceTree) {
        use relon_analyzer::workspace::{
            analyze_entry, LoadError, LoadedModule, ModuleLoader,
        };
        use std::collections::HashMap;
        use std::path::PathBuf;

        struct MapLoader {
            files: HashMap<String, (String, String)>,
        }
        impl ModuleLoader for MapLoader {
            fn load(
                &mut self,
                path: &str,
                _current_dir: &std::path::Path,
            ) -> Result<LoadedModule, LoadError> {
                match self.files.get(path) {
                    Some((canon, source)) => Ok(LoadedModule {
                        canonical_id: canon.clone(),
                        source: source.clone(),
                        current_dir: PathBuf::from("/abs"),
                    }),
                    None => Err(LoadError::NotFound),
                }
            }
        }
        let mut files: HashMap<String, (String, String)> = HashMap::new();
        for (raw, canonical, content) in loader_files {
            files.insert(
                (*raw).to_string(),
                ((*canonical).to_string(), (*content).to_string()),
            );
        }
        let mut loader = MapLoader { files };
        let workspace = analyze_entry(
            entry_id.to_string(),
            entry_source,
            PathBuf::from("/abs"),
            &mut loader,
        );
        let entry_tree = workspace
            .modules
            .get(entry_id)
            .cloned()
            .expect("entry analyzed");
        let entry_node = workspace
            .nodes
            .get(entry_id)
            .cloned()
            .expect("entry parsed");
        let entry = DocumentEntry {
            source: entry_source.to_string(),
            root: entry_node,
            tree: entry_tree,
        };
        (entry, workspace)
    }

    #[test]
    fn cross_module_alias_call_jumps_to_imported_field() {
        // `lib.shout(...)` — cursor on `shout` should land in lib.relon
        // on the closure value bound to `shout:`.
        let entry_source = r#"#import lib from "./lib"
{ greeting: lib.shout("hi") }"#;
        let lib_source = r#"{ shout(s): s + "!" }"#;
        let (entry, ws) = workspace_entry(
            "/abs/entry",
            entry_source,
            &[("./lib", "/abs/lib", lib_source)],
        );
        // Find the `shout` token's column on line 1 (0-indexed). It
        // sits inside `lib.shout(...)`; we anchor on the 's'.
        let line2 = entry_source.lines().nth(1).unwrap();
        let col = line2.find("shout").unwrap() as u32;
        let pos = Position {
            line: 1,
            character: col,
        };
        let uri = Url::parse("file:///abs/entry").unwrap();
        let loc = resolve(&entry, pos, &uri, Some(&ws)).expect("cross-file resolve");
        assert_eq!(
            loc.uri.as_str(),
            "file:///abs/lib",
            "should land on the imported module's URI"
        );
        // The target's range should cover the closure value, which
        // starts after the `shout(s):` prefix.
        assert!(
            loc.range.start.line == 0,
            "target should be on lib.relon line 0"
        );
    }

    #[test]
    fn cross_module_alias_head_alone_jumps_to_module_start() {
        // Bare `lib` (no tail) — cursor on `lib` should jump to the
        // start of the imported file.
        let entry_source = r#"#import lib from "./lib"
{ passthrough: lib }"#;
        let lib_source = r#"{ x: 1 }"#;
        let (entry, ws) = workspace_entry(
            "/abs/entry",
            entry_source,
            &[("./lib", "/abs/lib", lib_source)],
        );
        let line2 = entry_source.lines().nth(1).unwrap();
        let col = line2.find("lib").unwrap() as u32;
        let pos = Position {
            line: 1,
            character: col,
        };
        let uri = Url::parse("file:///abs/entry").unwrap();
        let loc = resolve(&entry, pos, &uri, Some(&ws)).expect("cross-file resolve");
        assert_eq!(loc.uri.as_str(), "file:///abs/lib");
        // Alias-head-alone semantics: range collapses to (0,0)..(0,0)
        // so the client just opens / focuses the imported file.
        assert_eq!(loc.range, Range::default());
    }

    #[test]
    fn cross_module_spread_call_jumps_to_imported_field() {
        // `#import * from "./lib"` + bare `shout(...)` should land on
        // lib.relon's `shout` closure.
        let entry_source = r#"#import * from "./lib"
{ v: shout("hi") }"#;
        let lib_source = r#"{ shout(s): s + "!" }"#;
        let (entry, ws) = workspace_entry(
            "/abs/entry",
            entry_source,
            &[("./lib", "/abs/lib", lib_source)],
        );
        let line2 = entry_source.lines().nth(1).unwrap();
        let col = line2.find("shout").unwrap() as u32;
        let pos = Position {
            line: 1,
            character: col,
        };
        let uri = Url::parse("file:///abs/entry").unwrap();
        let loc = resolve(&entry, pos, &uri, Some(&ws)).expect("cross-file resolve");
        assert_eq!(loc.uri.as_str(), "file:///abs/lib");
    }

    #[test]
    fn same_file_fn_call_jumps_to_local_definition() {
        // Regression: same-file `multiply(a, b)` call should still
        // resolve to the local closure definition. Confirms FnCall
        // path resolution didn't break the single-file path.
        let src = r#"{
    multiply(a, b): a * b,
    result: multiply(2, 3)
}"#;
        let entry = entry(src);
        let line = src.lines().nth(2).unwrap();
        // Find the call-site `multiply` (the one with `(2, 3)`).
        let col = line.find("multiply").unwrap() as u32;
        let pos = Position {
            line: 2,
            character: col,
        };
        let uri = Url::parse("file:///x.relon").unwrap();
        let loc = resolve(&entry, pos, &uri, None).expect("must resolve");
        assert_eq!(loc.uri, uri);
        // Target should sit on line 1 (the definition line).
        assert_eq!(loc.range.start.line, 1);
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
            resolve(&entry, pos, &uri, None).is_none(),
            "std/ paths shouldn't produce a file Location"
        );
    }
}
