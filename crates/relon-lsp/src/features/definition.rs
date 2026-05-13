//! `textDocument/definition`.
//!
//! Thin LSP adapter over [`relon_analyzer::goto_def::resolve`]. The
//! analyzer module owns the cursor walking, position math, and
//! same-file / cross-file resolution rules; this handler converts the
//! resulting [`GotoTarget`] into `lsp_types::Location`.

use crate::server::DocumentEntry;
use lsp_types::{Location, Position, Range, Url};
use relon_analyzer::goto_def::{self, GotoTarget};
use relon_analyzer::WorkspaceTree;

/// Resolve `position` (in `entry`) to a definition location. The
/// returned `Location` carries `uri` for in-document hits and a URI
/// derived from the imported module's canonical id (a filesystem
/// path) for cross-file hits.
pub fn resolve(
    entry: &DocumentEntry,
    position: Position,
    uri: &Url,
    workspace: Option<&WorkspaceTree>,
) -> Option<Location> {
    // The analyzer needs to know which module the cursor is in so it
    // can find the `import_graph` slot for the import-path-literal
    // case. We derive a canonical id from the URI when it's a file://;
    // for non-file URIs (untitled, synthetic) we pass None and the
    // analyzer's path-literal lookup falls back to "no canonical id".
    let entry_id = uri
        .to_file_path()
        .ok()
        .and_then(|p| std::fs::canonicalize(&p).ok())
        .map(|p| p.to_string_lossy().to_string());

    let target = goto_def::resolve(
        &entry.source,
        &entry.root,
        &entry.tree,
        workspace,
        entry_id.as_deref(),
        position.line,
        position.character,
    )?;

    match target {
        GotoTarget::Node {
            module_id,
            start,
            end,
        } => {
            let target_uri = match module_id.as_deref() {
                Some(id) => module_id_to_uri(id)?,
                None => uri.clone(),
            };
            let source = match module_id.as_deref() {
                Some(id) => workspace
                    .and_then(|ws| {
                        // The workspace pass doesn't retain raw source
                        // text; we re-read it for non-entry modules so
                        // offset → (line, col) translation works. Tiny
                        // I/O cost (one read per goto-def), acceptable
                        // for an interactive request.
                        if let Ok(text) = std::fs::read_to_string(id) {
                            return Some(text);
                        }
                        ws.modules.get(id).map(|_| String::new())
                    })
                    .unwrap_or_default(),
                None => entry.source.clone(),
            };
            // Range::default() (0..0) is the "alias head alone" /
            // "module-only" jump signal coming out of the analyzer;
            // pass it through verbatim so the LSP client just opens
            // the target file at the top.
            let range = if start == 0 && end == 0 {
                Range::default()
            } else {
                let start_pos = position_from_offset(&source, start);
                let end_pos = position_from_offset(&source, end);
                Range {
                    start: start_pos,
                    end: end_pos,
                }
            };
            Some(Location {
                uri: target_uri,
                range,
            })
        }
        GotoTarget::ImportPath {
            raw_path,
            canonical_id,
        } => {
            // Prefer the canonical id the workspace already resolved.
            // Fall back to the legacy URI-join behaviour for clients
            // running without a workspace pass (single-file LSP mode).
            if let Some(id) = canonical_id {
                return module_id_to_uri(&id).map(|target_uri| Location {
                    uri: target_uri,
                    range: Range::default(),
                });
            }
            if raw_path.starts_with("std/") {
                return None;
            }
            let target_uri = uri.join(&raw_path).ok()?;
            Some(Location {
                uri: target_uri,
                range: Range::default(),
            })
        }
    }
}

/// Re-export of `offset_to_position` shaped for the `lsp_types::Position`
/// return type that the rest of the LSP layer expects.
fn position_from_offset(source: &str, offset: usize) -> Position {
    let (line, character) = goto_def::offset_to_position(source, offset);
    Position { line, character }
}

/// Translate a canonical module id back into an LSP URI. Filesystem
/// paths round-trip through `Url::from_file_path`; synthetic ids
/// (`std/...`, in-memory playground keys) get a fallback scheme.
fn module_id_to_uri(canonical_id: &str) -> Option<Url> {
    if let Ok(uri) = Url::from_file_path(canonical_id) {
        return Some(uri);
    }
    Url::parse(&format!("relon-module:///{canonical_id}")).ok()
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
