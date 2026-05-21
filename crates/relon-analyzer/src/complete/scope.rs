//! Complete sub-module: scope-walking candidate collectors.
//!
//! These helpers walk the AST from the root toward the cursor offset,
//! collecting in-scope bindings (closure params, sibling pair keys,
//! enclosing dict pair keys, comprehension iteration variables) so the
//! Bare / Decorator / Member contexts can offer name-based completions.
//!
//! Shared utility:
//!
//! * [`is_inside_list`] — does the cursor sit inside a List or
//!   Comprehension? Drives the gating of iteration-only reference vars.
//! * [`collect_callable_pairs_in_scope`] / [`find_named_in_scope`] —
//!   variants used by decorator and partial-AST member-access
//!   collectors when they need either the param list or the value
//!   node of a named pair.

use super::{children_of, contains_offset, CompletionItem, CompletionKind};
use crate::tree::AnalyzedTree;
use relon_parser::{Expr, Node, TokenKey};

/// Mirror of [`push_scope_candidates`] for the partial-AST path.
/// Same scope walk, just without the unused `AnalyzedTree` argument
/// that the workspace-aware `resolve` threads through. Kept separate
/// so the recovering entry never reaches into analyzer-internal
/// machinery a partial parse couldn't populate.
pub(super) fn push_scope_candidates_partial(
    items: &mut Vec<CompletionItem>,
    root: &Node,
    offset: usize,
) {
    walk_scope(root, offset, items);
}

/// Walks the AST from the root toward `offset`, accumulating in-scope
/// names. Innermost names land last; the dedupe pass keeps the first
/// insertion of each `(label, kind)` so outer names are preferred for
/// the visible kind label, but both still appear once.
pub(super) fn push_scope_candidates(
    items: &mut Vec<CompletionItem>,
    root: &Node,
    tree: &AnalyzedTree,
    offset: usize,
) {
    let _ = tree;
    walk_scope(root, offset, items);
}

fn walk_scope(node: &Node, offset: usize, items: &mut Vec<CompletionItem>) {
    if !contains_offset(node, offset) {
        return;
    }

    // Each enclosing Closure contributes its parameter bindings.
    if let Expr::Closure { params, body, .. } = &*node.expr {
        for p in params {
            items.push(CompletionItem {
                label: p.name.clone(),
                kind: CompletionKind::Parameter,
                detail: Some("param".to_string()),
                apply_snippet: None,
            });
        }
        // Closure body might itself contain a Dict / Closure / etc.
        walk_scope(body, offset, items);
        return;
    }

    // Each enclosing Dict contributes its pair keys.
    if let Expr::Dict(pairs) = &*node.expr {
        for (key, value) in pairs {
            if let TokenKey::String(name, _, _) = key {
                let kind = if matches!(&*value.expr, Expr::Closure { .. }) {
                    CompletionKind::Method
                } else {
                    CompletionKind::Field
                };
                let detail = if matches!(kind, CompletionKind::Method) {
                    Some("method".to_string())
                } else {
                    Some("field".to_string())
                };
                items.push(CompletionItem {
                    label: name.clone(),
                    kind,
                    detail,
                    apply_snippet: None,
                });
            }
        }
        // Recurse into whichever pair's value contains the cursor.
        for (_, value) in pairs {
            if contains_offset(value, offset) {
                walk_scope(value, offset, items);
            }
        }
        return;
    }

    // Comprehension introduces the iteration variable into scope.
    if let Expr::Comprehension {
        id,
        element,
        iterable,
        condition,
    } = &*node.expr
    {
        items.push(CompletionItem {
            label: id.clone(),
            kind: CompletionKind::Parameter,
            detail: Some("for-binding".to_string()),
            apply_snippet: None,
        });
        let candidates: [Option<&Node>; 3] = [Some(element), Some(iterable), condition.as_ref()];
        for child in candidates.into_iter().flatten() {
            if contains_offset(child, offset) {
                walk_scope(child, offset, items);
            }
        }
        return;
    }

    // Default: descend into every child whose range covers the cursor.
    // This handles Binary / Ternary / FnCall / FString / etc.
    for child in children_of(node) {
        if contains_offset(child, offset) {
            walk_scope(child, offset, items);
        }
    }
}

/// Walks the AST and returns `true` when the cursor sits inside a
/// `List(...)` or `Comprehension(...)` expression. Drives the gating
/// of iteration-only reference vars (`&prev`, `&next`, `&index`).
pub(super) fn is_inside_list(root: &Node, offset: usize) -> bool {
    fn visit(node: &Node, offset: usize, in_list: bool) -> bool {
        if !covers(node, offset) {
            return false;
        }
        let here = matches!(&*node.expr, Expr::List(_) | Expr::Comprehension { .. });
        let nested = in_list || here;
        for c in crate::goto_def::smallest_node_at(node, offset).into_iter() {
            // unused — we just need a deep walk; do recursion manually.
            let _ = c;
        }
        // Manual recursion via the child-walker that handles directive
        // bodies + decorator args, mirroring resolve.rs's scope walker.
        for child in children_of(node) {
            if visit(child, offset, nested) {
                return true;
            }
        }
        nested && covers(node, offset)
    }
    fn covers(node: &Node, offset: usize) -> bool {
        node.range.start.offset <= offset && offset <= node.range.end.offset
    }
    visit(root, offset, false)
}

/// Walk scope around the cursor collecting `(name, params)` for every
/// closure-valued Dict pair in scope. Mirrors the Method portion of
/// `walk_scope` but preserves the param list — needed for snippet
/// expansion (decorator / member-method completion).
pub(super) fn collect_callable_pairs_in_scope(
    root: &Node,
    offset: usize,
) -> Vec<(String, Vec<String>)> {
    fn visit(node: &Node, offset: usize, out: &mut Vec<(String, Vec<String>)>) {
        if !contains_offset(node, offset) {
            return;
        }
        if let Expr::Dict(pairs) = &*node.expr {
            for (key, value) in pairs {
                if let TokenKey::String(name, _, _) = key {
                    if let Expr::Closure { params, .. } = &*value.expr {
                        let param_names: Vec<String> =
                            params.iter().map(|p| p.name.clone()).collect();
                        out.push((name.clone(), param_names));
                    }
                }
            }
            for (_, value) in pairs {
                if contains_offset(value, offset) {
                    visit(value, offset, out);
                }
            }
            return;
        }
        if let Expr::Closure { body, .. } = &*node.expr {
            visit(body, offset, out);
            return;
        }
        for child in children_of(node) {
            if contains_offset(child, offset) {
                visit(child, offset, out);
            }
        }
    }
    let mut out = Vec::new();
    visit(root, offset, &mut out);
    out
}

/// Search the AST from `root` for a Dict pair whose key matches
/// `name`, visible from the cursor at `offset`. Walks outward from
/// the innermost enclosing scope so a closer sibling shadows a
/// farther one — same scoping rules as `walk_scope` reads.
pub(super) fn find_named_in_scope<'a>(
    root: &'a Node,
    name: &str,
    offset: usize,
) -> Option<&'a Node> {
    // Inner: visit `node` if it covers the cursor, descending into
    // whichever child contains the offset; on the way back up, check
    // each enclosing Dict's pairs. Returns the closest matching pair
    // value.
    fn visit<'a>(node: &'a Node, name: &str, offset: usize) -> Option<&'a Node> {
        if !node_covers(node, offset) {
            return None;
        }
        // Descend first so inner scopes return their match before the
        // outer scope is consulted.
        if let Expr::Dict(pairs) = &*node.expr {
            for (_, value) in pairs {
                if let Some(inner) = visit(value, name, offset) {
                    return Some(inner);
                }
            }
            for (key, value) in pairs {
                if let TokenKey::String(k, _, _) = key {
                    if k == name {
                        return Some(value);
                    }
                }
            }
            return None;
        }
        if let Expr::Closure { body, .. } = &*node.expr {
            if let Some(inner) = visit(body, name, offset) {
                return Some(inner);
            }
            return None;
        }
        // Generic descent for everything else.
        for child in super::children_of(node) {
            if let Some(inner) = visit(child, name, offset) {
                return Some(inner);
            }
        }
        None
    }
    fn node_covers(node: &Node, offset: usize) -> bool {
        node.range.start.offset <= offset && offset <= node.range.end.offset
    }
    visit(root, name, offset)
}
