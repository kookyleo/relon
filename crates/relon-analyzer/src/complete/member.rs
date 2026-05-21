//! Complete sub-module: `ident.X` member-access candidates.
//!
//! Two variants:
//!
//! * [`push_member_candidates`] — strict path. `head` is looked up in
//!   the analyzed tree's `imports` table; when it names a module
//!   alias, the candidates come from the imported module's root Dict
//!   (via the workspace tree).
//! * [`push_member_candidates_partial`] — partial-AST path. No
//!   workspace available, so we walk the current document's AST
//!   looking for a Dict pair whose key matches `head` (`parent.│`
//!   where `parent` is a sibling dict) and surface its inner pair
//!   keys.

use super::scope::find_named_in_scope;
use super::{keywords::call_snippet, CompletionItem, CompletionKind};
use crate::tree::AnalyzedTree;
use crate::workspace::WorkspaceTree;
use relon_parser::{Expr, Node, TokenKey};

/// `lib.X` completion. `head` is the segment before the dot; we look
/// it up in `tree.imports` and, when it's an alias for another module,
/// pull that module's top-level dict pair keys out of `workspace`.
/// Partial-AST member-access completion. When the user types
/// `name.│`, walk the AST from the root toward the cursor looking
/// for a Dict pair whose key is `head`. If the matching value is
/// itself a Dict, surface every key as a Field / Method candidate.
/// Falls back silently when `head` doesn't resolve — the caller
/// won't see noise from speculative siblings.
pub(super) fn push_member_candidates_partial(
    items: &mut Vec<CompletionItem>,
    root: &Node,
    head: &str,
    offset: usize,
) {
    if let Some(target) = find_named_in_scope(root, head, offset) {
        if let Expr::Dict(pairs) = &*target.expr {
            for (key, value) in pairs {
                if let TokenKey::String(name, _, _) = key {
                    let (kind, detail, apply_snippet) = match &*value.expr {
                        Expr::Closure { params, .. } => {
                            let param_names: Vec<String> =
                                params.iter().map(|p| p.name.clone()).collect();
                            (
                                CompletionKind::Method,
                                Some("method".to_string()),
                                Some(call_snippet(name, &param_names)),
                            )
                        }
                        _ => (CompletionKind::Field, Some("field".to_string()), None),
                    };
                    items.push(CompletionItem {
                        label: name.clone(),
                        kind,
                        detail,
                        apply_snippet,
                    });
                }
            }
        }
    }
}

pub(super) fn push_member_candidates(
    items: &mut Vec<CompletionItem>,
    head: &str,
    tree: &AnalyzedTree,
    workspace: Option<&WorkspaceTree>,
) {
    // Reference base prefixes (`&root.X` etc.) flow through here when
    // the cursor's prev byte is `.` but the head starts with `&`. We
    // *could* offer ancestor Dict pair keys, but for v1 we only do
    // module-alias member access.

    let import = tree
        .imports
        .iter()
        .find(|imp| imp.alias.as_deref() == Some(head));
    let Some(import) = import else {
        return;
    };
    let Some(path) = &import.path else {
        return;
    };
    let Some(ws) = workspace else {
        return;
    };

    // Look up the imported module's analyzed tree by trying the path
    // directly first; if that misses, try the import graph for a key
    // that ends with the path string.
    let module_id = if ws.modules.contains_key(path) {
        path.clone()
    } else {
        match ws
            .modules
            .keys()
            .find(|k| k.ends_with(path.trim_start_matches("./")))
        {
            Some(k) => k.clone(),
            None => return,
        }
    };

    let module_root = match ws.nodes.get(&module_id) {
        Some(r) => r,
        None => return,
    };

    if let Expr::Dict(pairs) = &*module_root.expr {
        for (key, value) in pairs {
            if let TokenKey::String(name, _, _) = key {
                let kind = if matches!(&*value.expr, Expr::Closure { .. }) {
                    CompletionKind::Method
                } else {
                    CompletionKind::Field
                };
                items.push(CompletionItem {
                    label: name.clone(),
                    kind,
                    detail: Some(format!("from {}", module_id)),
                    apply_snippet: None,
                });
            }
        }
    }
}
