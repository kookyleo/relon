//! Map LSP cursor positions to AST nodes.
//!
//! [`smallest_node_at`] walks the AST and returns the deepest node whose
//! range covers a given byte offset — used by every feature handler to
//! answer "what is the user pointing at?". Position translation lives
//! one module up in `crate::position`.

use relon_parser::{
    CallArg, Decorator, Directive, DirectiveBody, Expr, FStringPart, Node, TokenKey, TokenRange,
};

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
/// args (so the cursor inside `#import path from "path"` lands on the path
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
