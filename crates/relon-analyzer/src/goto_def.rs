//! Cursor-driven definition lookup, factored out of `relon-lsp` so the
//! WASM playground can drive the same resolver without pulling in
//! `lsp-types` / `lsp-server`. The LSP feature handler is now a thin
//! adapter that wraps `GotoTarget` in `lsp_types::Location`.
//!
//! Positions are plain `(line, character)` pairs where `character` is a
//! UTF-16 code-unit index (matching LSP and WASM conventions). The
//! `position_to_offset` helper does the UTF-16 → UTF-8 byte mapping;
//! callers don't have to.

use crate::tree::AnalyzedTree;
use crate::workspace::WorkspaceTree;
use relon_parser::{
    CallArg, Decorator, Directive, DirectiveBody, Expr, FStringPart, Node, NodeId, TokenKey,
    TokenRange,
};

/// Result of a goto-definition query.
#[derive(Debug, Clone)]
pub enum GotoTarget {
    /// Reference resolved to a value node in a known module.
    Node {
        /// Target module's canonical id. `None` means "same module as
        /// the query" (so the caller reuses the input document's URI /
        /// path); `Some` means a different file the workspace knows
        /// about, identified by canonical id.
        module_id: Option<String>,
        /// Byte range of the target value node in its module source.
        /// LSP / WASM callers map this back to line/column themselves.
        start: usize,
        end: usize,
    },
    /// Cursor sits on the path string of a `#import x from "..."`
    /// directive. The caller decides how to translate the path into
    /// the platform's location type — LSP joins it against the
    /// document URI, WASM looks it up in the in-memory sources map.
    ImportPath {
        /// Raw path string written in the directive.
        raw_path: String,
        /// Canonical id if the workspace resolved this import; `None`
        /// when no workspace was supplied or the resolution failed.
        canonical_id: Option<String>,
    },
}

/// Convert an LSP-style `(line, utf16_character)` position into a
/// UTF-8 byte offset against `source`. Tolerant of out-of-range
/// positions: clamps to the source length so it never panics on bad
/// caller input.
pub fn position_to_offset(source: &str, line: u32, character: u32) -> usize {
    let target_line = line;
    let target_char = character;
    let mut cur_line = 0u32;
    let mut cur_char = 0u32;
    let mut byte = 0;
    for ch in source.chars() {
        if cur_line == target_line && cur_char >= target_char {
            return byte;
        }
        let len = ch.len_utf8();
        if ch == '\n' {
            if cur_line == target_line {
                return byte;
            }
            cur_line += 1;
            cur_char = 0;
        } else {
            cur_char += ch.len_utf16() as u32;
        }
        byte += len;
    }
    source.len()
}

/// Map a UTF-8 byte offset back to `(line, utf16_character)`. Returns
/// `(0, 0)` for out-of-range offsets and for offsets inside a
/// multi-byte char (clamped to the char's start).
pub fn offset_to_position(source: &str, offset: usize) -> (u32, u32) {
    let offset = offset.min(source.len());
    let mut line = 0u32;
    let mut character = 0u32;
    let mut byte = 0;
    for ch in source.chars() {
        if byte >= offset {
            break;
        }
        let len = ch.len_utf8();
        if byte + len > offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
        byte += len;
    }
    (line, character)
}

/// True iff `offset` falls inside `range` (start inclusive, end
/// inclusive — so cursors right at the closing brace still bind to
/// the surrounding node).
pub fn covers(range: TokenRange, offset: usize) -> bool {
    offset >= range.start.offset && offset <= range.end.offset
}

/// Walk `root` and return the deepest `Node` whose `range` covers
/// `offset`. Returns `root` itself when nothing more specific covers
/// the cursor (so callers can always assume `Some` for an in-bounds
/// offset).
pub fn smallest_node_at(root: &Node, offset: usize) -> Option<&Node> {
    if !covers(root.range, offset) {
        return None;
    }
    let mut best = root;
    walk(root, offset, &mut best);
    Some(best)
}

fn walk<'a>(node: &'a Node, offset: usize, best: &mut &'a Node) {
    if !covers(node.range, offset) {
        return;
    }
    if range_size(node.range) < range_size(best.range) {
        *best = node;
    }
    for child in children(node) {
        walk(child, offset, best);
    }
}

fn range_size(range: TokenRange) -> usize {
    range.end.offset.saturating_sub(range.start.offset)
}

/// Yield expression-shaped children plus decorator argument values.
/// Mirrors the analyzer's walker with the addition of decorator args
/// (so the cursor inside `#import path from "path"` lands on the path
/// literal node).
fn children(node: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    for dec in &node.decorators {
        push_decorator_children(dec, &mut out);
    }
    for dir in &node.directives {
        push_directive_children(dir, &mut out);
    }
    match &*node.expr {
        Expr::Dict(pairs) => {
            for (key, value) in pairs {
                if let TokenKey::Dynamic(inner, _) = key {
                    out.push(inner);
                }
                out.push(value);
            }
        }
        Expr::List(items) => out.extend(items.iter()),
        Expr::Spread(inner) => out.push(inner),
        Expr::Comprehension {
            element,
            iterable,
            condition,
            ..
        } => {
            out.push(element);
            out.push(iterable);
            if let Some(cond) = condition {
                out.push(cond);
            }
        }
        Expr::Binary(_, l, r) => {
            out.push(l);
            out.push(r);
        }
        Expr::Unary(_, inner) => out.push(inner),
        Expr::Ternary { cond, then, els } => {
            out.push(cond);
            out.push(then);
            out.push(els);
        }
        Expr::FnCall { args, .. } => {
            for arg in args {
                out.push(&arg.value);
            }
        }
        Expr::FString(parts) => {
            for part in parts {
                if let FStringPart::Interpolation(n) = part {
                    out.push(n);
                }
            }
        }
        Expr::Where { expr, bindings } => {
            out.push(expr);
            out.push(bindings);
        }
        Expr::Match { expr, arms } => {
            out.push(expr);
            for (pat, body) in arms {
                out.push(pat);
                out.push(body);
            }
        }
        Expr::Closure { body, .. } => out.push(body),
        Expr::VariantCtor { body, .. } => out.push(body),
        Expr::Reference { .. }
        | Expr::Variable(_)
        | Expr::Type(_)
        | Expr::Wildcard
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::String(_) => {}
    }
    out
}

fn push_decorator_children<'a>(dec: &'a Decorator, out: &mut Vec<&'a Node>) {
    for CallArg { value, .. } in &dec.args {
        out.push(value);
    }
}

fn push_directive_children<'a>(dir: &'a Directive, out: &mut Vec<&'a Node>) {
    match &dir.body {
        DirectiveBody::Value(body) => out.push(body),
        DirectiveBody::NameBody { body, .. } => out.push(body),
        DirectiveBody::Bare | DirectiveBody::Import { .. } | DirectiveBody::Main { .. } => {}
    }
}

/// Resolve `(line, character)` in `entry_source` (with already-parsed
/// `entry_root` + analyzed `entry_tree`) to a definition target.
///
/// Cases, in order:
///
/// 1. Cursor on an `#import path from "..."` path string —
///    `GotoTarget::ImportPath` with the raw path + canonical id (when
///    a workspace is supplied).
/// 2. Cursor on a node with a cross-module ref (requires `workspace`)
///    — `GotoTarget::Node { module_id: Some, .. }`.
/// 3. Cursor on a node with an in-document reference —
///    `GotoTarget::Node { module_id: None, .. }`.
/// 4. Otherwise — `None`.
pub fn resolve(
    entry_source: &str,
    entry_root: &Node,
    entry_tree: &AnalyzedTree,
    workspace: Option<&WorkspaceTree>,
    entry_module_id: Option<&str>,
    line: u32,
    character: u32,
) -> Option<GotoTarget> {
    let offset = position_to_offset(entry_source, line, character);

    // (1) #import path literal.
    if let Some(target) = import_path_target(entry_tree, offset, workspace, entry_module_id) {
        return Some(target);
    }

    let node = smallest_node_at(entry_root, offset)?;
    match &*node.expr {
        Expr::Reference { .. } | Expr::Variable(_) | Expr::FnCall { .. } => {}
        _ => return None,
    }

    // (2) Cross-module ref: takes precedence over same-doc references
    //     (it's only populated when the in-doc walk missed). Requires
    //     a workspace so we can grab the target module's parsed tree
    //     and the node's source range.
    if let Some(ws) = workspace {
        if let Some(cross) = entry_tree.cross_module_references.get(&node.id) {
            // Walk the cursor expression's path tail through the
            // target module's Dict structure, the same way same-file
            // refs walk path tails. For alias imports the post-pass
            // already consumed `tail[0]` into `cross.target`, so we
            // continue from `tail[1..]`; for destructure / spread the
            // target *is* the head binding, so we use the full tail.
            let full_tail = path_tail_of(&node.expr);
            let extra_tail = match cross.via {
                crate::resolve::CrossModuleVia::Alias => &full_tail[full_tail.len().min(1)..],
                _ => &full_tail[..],
            };
            return cross_module_target(ws, cross, extra_tail);
        }
    }

    // (3) Same-document reference. `references` resolves only the
    //     path's head (a known v1 simplification — see resolve.rs);
    //     for go-to-definition we want to land on the deepest field
    //     the path can statically reach, the way IDEs do. So we
    //     re-walk the original cursor node's path tail through the
    //     head's value-node Dict structure.
    let resolved = entry_tree.references.get(&node.id)?;
    let head_node = entry_tree.node_index.get(&resolved.target)?;
    let path_tail = path_tail_of(&node.expr);
    let deepest = walk_path_tail(head_node, &path_tail);
    Some(GotoTarget::Node {
        module_id: None,
        start: deepest.range.start.offset,
        end: deepest.range.end.offset,
    })
}

/// Extract the path tail (segments after the head) for a reference
/// expression. Returns an empty slice for non-reference shapes so
/// callers can treat "no tail" uniformly with "head-only path".
fn path_tail_of(expr: &Expr) -> Vec<String> {
    let path: &[TokenKey] = match expr {
        Expr::Reference { path, .. } => path,
        Expr::Variable(path) => path,
        Expr::FnCall { path, .. } => path,
        _ => return Vec::new(),
    };
    path.iter()
        .skip(1)
        .map_while(|seg| match seg {
            TokenKey::String(s, _, _) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// Walk `start` deeper using the path tail, descending into each Dict
/// child whose key matches. Returns the deepest reached node — if any
/// segment misses (non-Dict intermediate, missing key, dynamic key),
/// we stop walking and surface the last node found, which still gives
/// the user a useful jump.
fn walk_path_tail<'a>(start: &'a Node, tail: &[String]) -> &'a Node {
    let mut current = start;
    for seg in tail {
        let Expr::Dict(pairs) = &*current.expr else {
            return current;
        };
        let next = pairs.iter().find_map(|(k, v)| match k {
            TokenKey::String(name, _, _) if name == seg => Some(v),
            _ => None,
        });
        match next {
            Some(child) => current = child,
            None => return current,
        }
    }
    current
}

fn cross_module_target(
    workspace: &WorkspaceTree,
    cross: &crate::resolve::CrossModuleRef,
    extra_tail: &[String],
) -> Option<GotoTarget> {
    let target_node_id: Option<NodeId> = cross.target;
    if let Some(target_id) = target_node_id {
        let target_tree = workspace.modules.get(&cross.module_id)?;
        let head_node = target_tree.node_index.get(&target_id)?;
        let deepest = walk_path_tail(head_node, extra_tail);
        return Some(GotoTarget::Node {
            module_id: Some(cross.module_id.clone()),
            start: deepest.range.start.offset,
            end: deepest.range.end.offset,
        });
    }
    // Alias head alone — point at the start of the target file.
    Some(GotoTarget::Node {
        module_id: Some(cross.module_id.clone()),
        start: 0,
        end: 0,
    })
}

fn import_path_target(
    tree: &AnalyzedTree,
    offset: usize,
    workspace: Option<&WorkspaceTree>,
    entry_module_id: Option<&str>,
) -> Option<GotoTarget> {
    for (idx, import) in tree.imports.iter().enumerate() {
        if !covers(import.range, offset) {
            continue;
        }
        let path = import.path.as_deref()?;
        let canonical_id = workspace
            .zip(entry_module_id)
            .and_then(|(ws, id)| ws.import_graph.get(id))
            .and_then(|edges| edges.get(idx))
            .cloned();
        return Some(GotoTarget::ImportPath {
            raw_path: path.to_string(),
            canonical_id,
        });
    }
    None
}
