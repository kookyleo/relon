//! Map LSP cursor positions to AST nodes.
//!
//! Two helpers:
//!
//! * [`position_to_offset`] — invert the LSP `(line, character)` pair
//!   back to a UTF-8 byte offset against the document source.
//! * [`smallest_node_at`] — walk the AST and return the deepest node
//!   whose range covers the offset. Used by every feature handler to
//!   answer "what is the user pointing at?".

use lsp_types::Position;
use relon_parser::{
    CallArg, Decorator, Expr, FStringPart, Node, TokenKey, TokenPosition, TokenRange,
};

/// Convert an LSP position (line + UTF-16 character) into a UTF-8 byte
/// offset against `source`. Mirrors the inverse helper in
/// `crate::diagnostics`. Tolerant of out-of-range positions: clamps to
/// the source length so we never produce a panic from bad client input.
pub fn position_to_offset(source: &str, position: Position) -> usize {
    let target_line = position.line;
    let target_char = position.character;
    let mut line = 0u32;
    let mut character = 0u32;
    let mut byte = 0;
    for ch in source.chars() {
        if line == target_line && character >= target_char {
            return byte;
        }
        let len = ch.len_utf8();
        if ch == '\n' {
            // If the cursor is past the end of `target_line`, snap to
            // the line break.
            if line == target_line {
                return byte;
            }
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
        byte += len;
    }
    source.len()
}

/// True iff `offset` falls inside `range` (start inclusive, end
/// inclusive — so cursors right at the closing brace still bind to
/// the surrounding node).
pub fn covers(range: TokenRange, offset: usize) -> bool {
    offset >= range.start.offset && offset <= range.end.offset
}

/// Walk `root` and return the deepest [`Node`] whose `range` covers
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
    // Prefer a smaller (more specific) range when it still covers
    // `offset`. Equal-range nodes keep the existing winner so we don't
    // bounce between siblings of identical span.
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
/// Mirrors the analyzer's walker, with the addition of decorator
/// args (so the cursor inside `@import("path")` lands on the path
/// literal node).
fn children(node: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
    for dec in &node.decorators {
        push_decorator_children(dec, &mut out);
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

/// Convert a parser `TokenPosition` to an LSP `Position`.
pub fn token_position(pos: TokenPosition) -> Position {
    // Parser uses 1-based lines/columns; LSP uses 0-based. `column`
    // is byte-aligned in the parser; we approximate by treating it as
    // a character index, which matches our `Position` math elsewhere.
    Position {
        line: pos.line.saturating_sub(1),
        character: (pos.column.saturating_sub(1)) as u32,
    }
}

/// Convert a parser `TokenRange` to an LSP `Range`.
pub fn token_range(range: TokenRange) -> lsp_types::Range {
    lsp_types::Range {
        start: token_position(range.start),
        end: token_position(range.end),
    }
}
