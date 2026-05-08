//! Module-graph pass.
//!
//! Scans every node's `#import <bindspec> from "..."` directives and
//! records each import as a [`ModuleImport`]. The pass does not actually
//! load any module — that's the evaluator's job at runtime — but it
//! gives LSP and CI tooling a static view of "what does this file
//! import" without invoking I/O.

use crate::directive_names::IMPORT;
use crate::tree::AnalyzedTree;
use relon_parser::{DirectiveBody, DirectiveImportSpec, Expr, Node, TokenRange};

/// Information about a single `#import` site.
#[derive(Debug, Clone)]
pub struct ModuleImport {
    /// First positional argument verbatim — the import path. `None` if
    /// the path is dynamic (reserved for future syntax; the current
    /// directive parser only accepts string-literal paths).
    pub path: Option<String>,
    /// `Some(name)` when the bindspec is a single alias
    /// (`#import string from "std/string"`); `None` when it's a spread
    /// (`#import *`) or destructure (`#import { a, b }`).
    pub alias: Option<String>,
    /// `true` when the bindspec is `*` — every exported binding from
    /// the module is brought into scope.
    pub spread: bool,
    /// Source range of the `#import ...` directive.
    pub range: TokenRange,
}

/// Walk the document for `#import` directives and record them. Nested
/// imports (directives on inner dict entries) are also captured so a
/// host can see every import site.
pub fn collect_imports(root: &Node, tree: &mut AnalyzedTree) {
    visit(root, tree);
}

fn visit(node: &Node, tree: &mut AnalyzedTree) {
    for dir in &node.directives {
        if dir.name == IMPORT {
            if let DirectiveBody::Import { spec, path, .. } = &dir.body {
                tree.imports.push(lower_import(spec, path, dir.range));
            }
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

fn lower_import(spec: &DirectiveImportSpec, path: &str, range: TokenRange) -> ModuleImport {
    let (alias, spread) = match spec {
        DirectiveImportSpec::Alias(name) => (Some(name.clone()), false),
        DirectiveImportSpec::Spread => (None, true),
        // Destructure list lives only on the AST today — it's lowered
        // to per-binding scope inserts by the evaluator. Surface "no
        // alias, not a spread" so existing consumers (LSP) still get a
        // meaningful module-import record.
        DirectiveImportSpec::Destructure(_) => (None, false),
    };
    ModuleImport {
        path: Some(path.to_string()),
        alias,
        spread,
        range,
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
            r#"#import list from "std/list"
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
            r#"#import * from "std/math"
            { halved: 10 / 2 }"#,
        );
        assert_eq!(tree.imports.len(), 1);
        assert!(tree.imports[0].spread);
    }
}
