//! Module-graph pass.
//!
//! Scans the root node's `@import("...")` decorators and records each
//! import as a [`ModuleImport`]. The pass does not actually load any
//! module — that's the evaluator's job at runtime — but it gives LSP
//! and CI tooling a static view of "what does this file import" without
//! invoking I/O.

use crate::decorator_names::{IMPORT, LIBRARY};
use crate::tree::AnalyzedTree;
use relon_parser::{Decorator, Expr, Node, TokenKey, TokenRange};

/// Information about a single `@import` site.
#[derive(Debug, Clone)]
pub struct ModuleImport {
    /// First positional argument verbatim — the import path. `None` if
    /// the path is dynamic (e.g. an interpolated f-string the analyzer
    /// won't try to evaluate).
    pub path: Option<String>,
    /// `as=` named argument when present.
    pub alias: Option<String>,
    /// `spread=true` flag.
    pub spread: bool,
    /// Source range of the `@import(...)` call.
    pub range: TokenRange,
}

/// Walk the root for `@import` decorators on the root expression and
/// record them. Nested imports (decorators on inner dict entries) are
/// also captured so a host can see every import site.
pub fn collect_imports(root: &Node, tree: &mut AnalyzedTree) {
    // Only the root-level `@library` marker counts; nested ones are data.
    tree.is_library = root.decorators.iter().any(is_library);
    visit(root, tree);
}

fn visit(node: &Node, tree: &mut AnalyzedTree) {
    for dec in &node.decorators {
        if is_import(dec) {
            tree.imports.push(lower_import(dec));
        }
    }
    match &*node.expr {
        Expr::Dict(pairs) => {
            for (_, value) in pairs {
                visit(value, tree);
            }
        }
        Expr::List(items) => {
            for item in items {
                visit(item, tree);
            }
        }
        _ => {}
    }
}

fn is_import(dec: &Decorator) -> bool {
    dec.path.len() == 1 && matches!(&dec.path[0], TokenKey::String(name, _, _) if name == IMPORT)
}

fn is_library(dec: &Decorator) -> bool {
    dec.path.len() == 1 && matches!(&dec.path[0], TokenKey::String(name, _, _) if name == LIBRARY)
}

fn lower_import(dec: &Decorator) -> ModuleImport {
    let mut path = None;
    let mut alias = None;
    let mut spread = false;

    for (idx, arg) in dec.args.iter().enumerate() {
        match arg.name.as_deref() {
            None if idx == 0 => {
                path = literal_string(&arg.value);
            }
            Some("as") => {
                alias = literal_string(&arg.value);
            }
            Some("spread") => {
                spread = literal_bool(&arg.value).unwrap_or(false);
            }
            _ => {}
        }
    }

    ModuleImport {
        path,
        alias,
        spread,
        range: dec.range,
    }
}

fn literal_string(node: &Node) -> Option<String> {
    if let Expr::String(s) = &*node.expr {
        Some(s.clone())
    } else {
        None
    }
}

fn literal_bool(node: &Node) -> Option<bool> {
    if let Expr::Bool(b) = &*node.expr {
        Some(*b)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use relon_parser::parse_document;

    fn analyze(src: &str) -> crate::AnalyzedTree {
        let node = parse_document(src).expect("parse");
        crate::analyze(&node)
    }

    #[test]
    fn collects_basic_import() {
        let tree = analyze(
            r#"@import("std/list", as="list")
            { ok: list.first([1, 2]) }"#,
        );
        assert_eq!(tree.imports.len(), 1);
        let m = &tree.imports[0];
        assert_eq!(m.path.as_deref(), Some("std/list"));
        assert_eq!(m.alias.as_deref(), Some("list"));
        assert!(!m.spread);
    }

    #[test]
    fn collects_spread_import() {
        let tree = analyze(
            r#"@import("std/math", spread=true)
            { halved: 10 / 2 }"#,
        );
        assert_eq!(tree.imports.len(), 1);
        assert!(tree.imports[0].spread);
    }

    #[test]
    fn dynamic_path_keeps_none_path() {
        // f-string path is dynamic; analyzer records the import site
        // but leaves `path` as `None`.
        let tree = analyze(
            r#"@import(f"std/list")
            {}"#,
        );
        assert_eq!(tree.imports.len(), 1);
        assert!(tree.imports[0].path.is_none());
    }
}
