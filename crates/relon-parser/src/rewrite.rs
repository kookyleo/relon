//! Shared, post-analyze AST pattern recognition for high-level rewrites.
//!
//! Historically the "fusion" rewrites (turning `list.sum(range(...))` and
//! friends into materialisation-free loops) lived only in the relon-IR
//! lowering layer (`relon-ir/src/lowering/peephole.rs`). That meant only the
//! compiled back-ends (cranelift / llvm / wasm) benefited; the tree-walk
//! interpreter still went through the stdlib and materialised the intermediate
//! `range(...)` list.
//!
//! This module hoists the *pattern recognition* half up to the AST level so
//! every downstream consumer can share it:
//!
//!   * the tree-walk interpreter matches a fused shape at eval-time and streams
//!     the result without ever building the intermediate list;
//!   * the IR lowering can delegate its own pattern check here instead of
//!     re-implementing the AST walk, keeping a single source of truth.
//!
//! The recognisers are intentionally *pure structural* matchers over the
//! parser AST. They carry NO semantic guarantee on their own — a match only
//! says "this expression has the recognised shape". Whether the rewrite is
//! *sound* (e.g. the range really produces `Int`s) is the caller's
//! responsibility and is why recognition is run **post-analyze**: the caller
//! already knows, from analyzer type info, that the receiver list is an
//! `Int` list.
//!
//! ## Reusable framework
//!
//! [`FusedPattern`] is the umbrella enum every recognised fusion lowers to.
//! Adding a future peephole means: add a variant, add its `match_*` recogniser,
//! and wire it into [`recognize_fused`]. Both the interpreter and the IR side
//! then pick the variant up for free. Today only [`FusedPattern::RangeSum`] is
//! implemented (the pilot); the eight remaining IR peepholes still live in
//! `relon-ir` and can migrate incrementally behind this same surface.

use crate::token::{CallArg, Expr, Node, TokenKey};

/// A high-level fusion pattern recognised at the AST level. Each variant
/// borrows the relevant sub-expressions straight off the AST so consumers can
/// re-evaluate / re-lower them in their own context (interpreter scope or IR
/// `LowerCtx`).
#[derive(Debug)]
pub enum FusedPattern<'a> {
    /// `list.sum(range(end))` or `list.sum(range(start, end))` with no
    /// intervening `.map(...)` / `.filter(...)` stages.
    ///
    /// Equivalent fused semantics (the byte-exact contract every back-end and
    /// the interpreter must honour):
    ///
    /// ```text
    /// acc: i64 = 0
    /// for i in start..end {        // empty when start >= end
    ///     acc = acc.wrapping_add(i)
    /// }
    /// result = Int(acc)
    /// ```
    ///
    /// This is a *fusion* (drop the intermediate `Vec<Value>` allocation), not
    /// a closed-form substitution: the same additions happen in the same order
    /// with the same wrapping-overflow behaviour as
    /// `[start, .., end-1].sum()`. `start` defaults to `0` for the one-arg
    /// `range(end)` form.
    RangeSum {
        /// `start` argument node when `range(start, end)`; `None` for the
        /// one-arg `range(end)` form (implicit `start = 0`).
        start: Option<&'a Node>,
        /// `end` argument node — always present.
        end: &'a Node,
    },
}

/// Recognise a fused high-level pattern in `expr`, if any.
///
/// Pure structural match; returns `None` when no recogniser fires (the caller
/// then falls through to its normal path). Run this **post-analyze**: a match
/// is necessary but not sufficient for soundness — the caller must already know
/// (from type info) that the rewrite preserves semantics for the operand
/// types at hand.
pub fn recognize_fused(expr: &Expr) -> Option<FusedPattern<'_>> {
    // Extension point: try each recogniser in turn. The first hit wins.
    match_range_sum(expr)
}

/// `list.sum(range(...))` (no map/filter chain).
///
/// Parser shape: `FnCall { path: [String("list"), String("sum")], args:
/// [ <range-call> ] }` where the single positional arg is a bare
/// `range(end)` / `range(start, end)` call.
fn match_range_sum(expr: &Expr) -> Option<FusedPattern<'_>> {
    let Expr::FnCall { path, args } = expr else {
        return None;
    };
    // Outer head must be `list.sum(<single positional arg>)`.
    if path.len() != 2 {
        return None;
    }
    if !matches!(&path[0], TokenKey::String(s, _, _) if s == "list") {
        return None;
    }
    if !matches!(&path[1], TokenKey::String(s, _, _) if s == "sum") {
        return None;
    }
    if args.len() != 1 || args[0].name.is_some() {
        return None;
    }
    let (start, end) = match_bare_range(&args[0].value.expr)?;
    Some(FusedPattern::RangeSum { start, end })
}

/// Recognise a bare `range(end)` / `range(start, end)` call, rejecting any
/// chain stage (`range(...).map(...)`) and any keyword arg. Returns the
/// optional `start` node and the mandatory `end` node.
fn match_bare_range(expr: &Expr) -> Option<(Option<&Node>, &Node)> {
    let Expr::FnCall { path, args } = expr else {
        return None;
    };
    if path.len() != 1 {
        return None;
    }
    if !matches!(&path[0], TokenKey::String(s, _, _) if s == "range") {
        return None;
    }
    if args.iter().any(arg_is_named) {
        return None;
    }
    match args.len() {
        1 => Some((None, &args[0].value)),
        2 => Some((Some(&args[0].value), &args[1].value)),
        _ => None,
    }
}

fn arg_is_named(a: &CallArg) -> bool {
    a.name.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_document;

    /// Pull the `#main` body expression out of a parsed document so the
    /// recognisers see exactly the call expression they would at eval /
    /// lowering time.
    fn body_expr(src: &str) -> Node {
        // Documents here are `#main(...) -> T\n<body>`; the parser returns the
        // body node as the document root's payload. We re-parse the bare body
        // expression to keep the test focused on the recogniser, not the
        // directive grammar.
        parse_document(src).expect("parse")
    }

    fn find_fncall(node: &Node) -> Option<Node> {
        if matches!(node.expr.as_ref(), Expr::FnCall { .. }) {
            return Some(node.clone());
        }
        for child in crate::child_nodes(node) {
            if let Some(found) = find_fncall(child) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn matches_range_end() {
        let doc = body_expr("#main(Int n) -> Int\nlist.sum(range(n))");
        let call = find_fncall(&doc).expect("fncall");
        let pat = recognize_fused(&call.expr).expect("match");
        match pat {
            FusedPattern::RangeSum { start, end: _ } => assert!(start.is_none()),
        }
    }

    #[test]
    fn matches_range_start_end() {
        let doc = body_expr("#main(Int n) -> Int\nlist.sum(range(5, n))");
        let call = find_fncall(&doc).expect("fncall");
        let pat = recognize_fused(&call.expr).expect("match");
        match pat {
            FusedPattern::RangeSum { start, end: _ } => assert!(start.is_some()),
        }
    }

    #[test]
    fn rejects_three_arg_range() {
        // `range` only takes 1 or 2 args; a 3-arg call must not match the
        // fused pattern (falls through to the normal error path).
        let doc = body_expr("#main(Int n) -> Int\nlist.sum(range(1, n, 2))");
        let call = find_fncall(&doc).expect("fncall");
        assert!(recognize_fused(&call.expr).is_none());
    }

    #[test]
    fn rejects_map_chain() {
        // A `.map(...)` stage means this is NOT the bare-range-sum pilot
        // pattern; the IR-side range-pipeline peephole still owns it.
        let doc = body_expr("#main(Int n) -> Int\nlist.sum(range(n).map((i) => i))");
        let call = find_fncall(&doc).expect("fncall");
        assert!(recognize_fused(&call.expr).is_none());
    }

    #[test]
    fn rejects_other_calls() {
        let doc = body_expr("#main(Int n) -> Int\nlist.max([1, 2, 3])");
        let call = find_fncall(&doc).expect("fncall");
        assert!(recognize_fused(&call.expr).is_none());
    }
}
