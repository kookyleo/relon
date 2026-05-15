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
    CallArg, Decorator, Directive, DirectiveBody, Expr, FStringPart, Node, NodeId, RefBase,
    TokenKey, TokenRange,
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

    // Per-segment resolution. For multi-segment paths the user
    // expects the jump to track *which* segment the cursor is on:
    //   &root.project.name
    //   ^      ^       ^
    //   |      |       └─ click here → `name`'s value
    //   |      └─ click here → `project`'s value (Dict on line 5)
    //   └─ click here → document root (`&root` base, line 2)
    let cursor_segment = locate_segment(&node.expr, offset);

    // (2) Cross-module ref: takes precedence over same-doc references
    //     (it's only populated when the in-doc walk missed). Requires
    //     a workspace so we can grab the target module's parsed tree
    //     and the node's source range.
    if let Some(ws) = workspace {
        if let Some(cross) = entry_tree.cross_module_references.get(&node.id) {
            // The post-pass consumed `tail[0]` into `cross.target` for
            // alias imports (so `lib.x` lands on `x`). For descent
            // we walk the cursor expression's `tail[1..=cursor_idx]`
            // inside the imported module's tree. For destructure /
            // spread the target *is* the head binding, so the tail
            // we walk starts at `[1..]` (skip head) just like the
            // same-file case.
            return cross_module_target(ws, cross, &node.expr, cursor_segment);
        }
    }

    // (3) Same-document reference. `references` resolves only the
    //     path's head (a known v1 simplification — see resolve.rs);
    //     for go-to-definition we descend through Dict children up
    //     to the segment the cursor is on. The Reference base
    //     prefix (`&root` / `&sibling` / `&uncle`) gets its own
    //     handling: jump to the *frame* the base would resolve into,
    //     not to the head field.
    if matches!(cursor_segment, SegmentLocation::Base) {
        if let Expr::Reference { base, .. } = &*node.expr {
            if let Some(target) = base_frame_jump(*base, entry_root, entry_tree, offset) {
                return Some(target);
            }
        }
    }
    let resolved = entry_tree.references.get(&node.id)?;
    let head_node = entry_tree.node_index.get(&resolved.target)?;
    let descent = descent_steps(&node.expr, cursor_segment);
    let (target, target_key_range) = walk_descent(head_node, &descent);
    // Prefer the key range so the editor lands on the *name* of the
    // field — VS Code's "click takes you to the symbol's identifier"
    // convention. Fall back to the value's range when (a) descent
    // didn't move (head case — look up via `field_key_ranges`) or
    // (b) the field's key wasn't a String literal (dynamic keys).
    let range = target_key_range
        .or_else(|| entry_tree.field_key_ranges.get(&resolved.target).copied())
        .map(|r| (r.start.offset, r.end.offset))
        .unwrap_or((target.range.start.offset, target.range.end.offset));
    Some(GotoTarget::Node {
        module_id: None,
        start: range.0,
        end: range.1,
    })
}

/// Compute the goto-def target for a Reference's base prefix
/// (`&root` / `&sibling` / `&uncle`). Each base resolves to a
/// different scope frame at runtime:
///
/// * `&root`    → the document's root Dict
/// * `&sibling` → the immediately-enclosing Dict
/// * `&uncle`   → the next Dict out from sibling
///
/// We mirror the runtime by scanning the AST for every Dict whose
/// range covers the cursor, outer-to-inner, then picking the right
/// frame by base. The jump lands on the owning *key* of that Dict
/// (e.g. `details:` for a `details: { ... }` field) when available,
/// falling back to the Dict's own opening brace when the Dict has no
/// owning key (the root, or a list-element dict).
fn base_frame_jump(
    base: RefBase,
    root: &Node,
    tree: &AnalyzedTree,
    offset: usize,
) -> Option<GotoTarget> {
    let dicts = enclosing_dicts(root, offset);
    let target = match base {
        // Root could legitimately be the same Dict as `dicts[0]`
        // (whole document), but we fall back to `root` even when no
        // Dict was matched — the document might be a list or atomic
        // expression at top level.
        RefBase::Root => dicts.first().copied().unwrap_or(root),
        RefBase::Sibling => *dicts.last()?,
        // `&uncle` skips one Dict frame. With < 2 enclosing Dicts the
        // reference would have stayed unresolved at analyze time too;
        // return None so the caller produces no jump.
        RefBase::Uncle => {
            if dicts.len() < 2 {
                return None;
            }
            dicts[dicts.len() - 2]
        }
        // List-context refs (`&prev`, `&next`, `&index`, `&this`) are
        // iteration-state dependent — no static target.
        _ => return None,
    };
    Some(jump_to_dict_anchor(target, tree))
}

/// Collect every Dict node whose range covers `offset`, outermost-to-
/// innermost. The cursor's own node is included only when it is itself
/// a Dict (rare for the base-prefix cases this helper supports).
fn enclosing_dicts(root: &Node, offset: usize) -> Vec<&Node> {
    let mut out = Vec::new();
    collect_enclosing_dicts(root, offset, &mut out);
    out
}

fn collect_enclosing_dicts<'a>(node: &'a Node, offset: usize, out: &mut Vec<&'a Node>) {
    if !covers(node.range, offset) {
        return;
    }
    if matches!(&*node.expr, Expr::Dict(_)) {
        out.push(node);
    }
    for child in children(node) {
        collect_enclosing_dicts(child, offset, out);
    }
}

/// Land on the most informative anchor for a Dict: its owning key
/// when it lives at `key: { ... }`, otherwise the Dict's opening
/// brace. Always returns the "same-document" form.
fn jump_to_dict_anchor(dict: &Node, tree: &AnalyzedTree) -> GotoTarget {
    if let Some(key_range) = tree.field_key_ranges.get(&dict.id) {
        return GotoTarget::Node {
            module_id: None,
            start: key_range.start.offset,
            end: key_range.end.offset,
        };
    }
    GotoTarget::Node {
        module_id: None,
        start: dict.range.start.offset,
        end: dict.range.start.offset,
    }
}

/// Which part of a path-bearing expression the cursor sits on. Used
/// to decide how deep to walk: clicking the second segment of
/// `a.b.c` should only descend to `b`, not all the way to `c`.
#[derive(Debug, Clone, Copy)]
enum SegmentLocation {
    /// Cursor before the first path segment — on the base prefix
    /// (`&root.`, `&sibling.`, ...) for a Reference, or implicitly
    /// the head for a Variable / FnCall when the cursor lands before
    /// path[0] (rare but observed at the leading `f"...${` of an
    /// interpolation start).
    Base,
    /// Cursor on path[i]. `0` is the head; deeper indices walk
    /// proportionally deeper through Dict children.
    Index(usize),
}

fn locate_segment(expr: &Expr, offset: usize) -> SegmentLocation {
    let path: &[TokenKey] = match expr {
        Expr::Reference { path, .. } => path,
        Expr::Variable(path) => path,
        Expr::FnCall { path, .. } => path,
        _ => return SegmentLocation::Base,
    };
    for (i, seg) in path.iter().enumerate() {
        let range = match seg {
            TokenKey::String(_, r, _) => *r,
            _ => continue,
        };
        if offset >= range.start.offset && offset <= range.end.offset {
            return SegmentLocation::Index(i);
        }
    }
    // Default: if the cursor doesn't land on any segment range
    // (whitespace / dots between segments, or before the first
    // segment), pick the segment immediately after the cursor so the
    // user still gets a sensible jump. Falls back to Base when
    // nothing in the path follows the cursor.
    for (i, seg) in path.iter().enumerate() {
        let range = match seg {
            TokenKey::String(_, r, _) => *r,
            _ => continue,
        };
        if range.start.offset > offset {
            return if i == 0 {
                SegmentLocation::Base
            } else {
                SegmentLocation::Index(i)
            };
        }
    }
    // Cursor past the last segment — Variables / FnCalls reach here
    // when the click hits the trailing `(` or paren contents (smallest
    // node still returns the FnCall). Treat as "deepest segment".
    let last = path
        .iter()
        .rposition(|seg| matches!(seg, TokenKey::String(_, _, _)));
    match last {
        Some(i) => SegmentLocation::Index(i),
        None => SegmentLocation::Base,
    }
}

/// Compute the Dict descent path (segment names after the head)
/// required to land on the cursor's segment. Returns an empty slice
/// when the cursor is at or before the head; longer slices for deeper
/// segments. The slice is consumed by `walk_descent` against the
/// head's value-node.
fn descent_steps(expr: &Expr, where_: SegmentLocation) -> Vec<String> {
    let path: &[TokenKey] = match expr {
        Expr::Reference { path, .. } => path,
        Expr::Variable(path) => path,
        Expr::FnCall { path, .. } => path,
        _ => return Vec::new(),
    };
    let stop = match where_ {
        SegmentLocation::Base => 0,
        SegmentLocation::Index(i) => i,
    };
    path.iter()
        .take(stop + 1)
        .skip(1)
        .map_while(|seg| match seg {
            TokenKey::String(s, _, _) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// Walk `start` deeper using the descent steps, descending into each
/// Dict child whose key matches. Returns the deepest node plus the
/// key-range of the *last* successful descent step (for caller-side
/// "select the key" highlighting). Empty descent → `(start, None)`.
/// Stops at the first non-Dict / missing key.
fn walk_descent<'a>(start: &'a Node, descent: &[String]) -> (&'a Node, Option<TokenRange>) {
    let mut current = start;
    let mut key_range = None;
    for seg in descent {
        let Expr::Dict(pairs) = &*current.expr else {
            return (current, key_range);
        };
        let mut next = None;
        for (k, v) in pairs {
            if let TokenKey::String(name, range, _) = k {
                if name == seg {
                    next = Some((v, *range));
                    break;
                }
            }
        }
        match next {
            Some((child, range)) => {
                current = child;
                key_range = Some(range);
            }
            None => return (current, key_range),
        }
    }
    (current, key_range)
}

fn cross_module_target(
    workspace: &WorkspaceTree,
    cross: &crate::resolve::CrossModuleRef,
    expr: &Expr,
    cursor_segment: SegmentLocation,
) -> Option<GotoTarget> {
    let target_node_id: Option<NodeId> = cross.target;
    // Cursor on the alias head itself (Base, or Index(0) for an
    // alias-style import): jump to the start of the imported file.
    // For destructure / spread imports, the head *is* the binding, so
    // Index(0) should still walk into the target field.
    let head_only = matches!(cursor_segment, SegmentLocation::Base)
        || (matches!(cursor_segment, SegmentLocation::Index(0))
            && matches!(cross.via, crate::resolve::CrossModuleVia::Alias));
    if head_only {
        return Some(GotoTarget::Node {
            module_id: Some(cross.module_id.clone()),
            start: 0,
            end: 0,
        });
    }
    let target_id = target_node_id?;
    let target_tree = workspace.modules.get(&cross.module_id)?;
    let head_node = target_tree.node_index.get(&target_id)?;
    // Build the extra descent: alias's `cross.target` already
    // consumed path[1], so we descend from path[2..=cursor_idx];
    // destructure / spread already mapped head to the target field
    // directly, so we descend from path[1..=cursor_idx].
    let descent = cross_module_descent(expr, cursor_segment, &cross.via);
    let (deepest, key_range) = walk_descent(head_node, &descent);
    let range = key_range
        .or_else(|| target_tree.field_key_ranges.get(&target_id).copied())
        .map(|r| (r.start.offset, r.end.offset))
        .unwrap_or((deepest.range.start.offset, deepest.range.end.offset));
    Some(GotoTarget::Node {
        module_id: Some(cross.module_id.clone()),
        start: range.0,
        end: range.1,
    })
}

fn cross_module_descent(
    expr: &Expr,
    cursor_segment: SegmentLocation,
    via: &crate::resolve::CrossModuleVia,
) -> Vec<String> {
    let path: &[TokenKey] = match expr {
        Expr::Reference { path, .. } => path,
        Expr::Variable(path) => path,
        Expr::FnCall { path, .. } => path,
        _ => return Vec::new(),
    };
    let stop = match cursor_segment {
        SegmentLocation::Base => return Vec::new(),
        SegmentLocation::Index(i) => i,
    };
    let start = match via {
        // alias.field → first segment already in cross.target
        crate::resolve::CrossModuleVia::Alias => 2,
        // bare imported binding (destructure / spread) → head is the
        // imported field; descend from path[1..]
        crate::resolve::CrossModuleVia::Destructured { .. }
        | crate::resolve::CrossModuleVia::Spread => 1,
    };
    if start > stop {
        return Vec::new();
    }
    path[start..=stop]
        .iter()
        .map_while(|seg| match seg {
            TokenKey::String(s, _, _) => Some(s.clone()),
            _ => None,
        })
        .collect()
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
