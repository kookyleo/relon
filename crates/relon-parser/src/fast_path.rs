//! v6-fix-D2-I cold-start parser fast-path.
//!
//! `parse_document_fast` recognises the narrow envelope used by the
//! `is_trivial_scalar_main` classifier:
//!
//! * exactly one `#main(<ScalarType> <ident>[, ...]) [-> <ScalarType>]`
//!   directive, no other directives, no decorators, no leading
//!   comments;
//! * a body that is a literal (Int / Float / Bool / String), a
//!   single-segment `Variable`, or a `Binary` / `Unary` / `Ternary`
//!   over those leaves (whitelisted operators only).
//!
//! For these shapes — overwhelmingly the W11 cold-start corpus and
//! every `#main(Int x) -> Int <arith>` config that lands in `--lite`
//! — the function builds a minimal [`Node`] directly from the source
//! bytes, **skipping** the rowan CST construction, the v2 lowering
//! pass, the `parse_leading_comments` walk, and decorator parsing
//! entirely. Anything outside the envelope returns `None`; the
//! caller falls back to [`crate::parse_document`].
//!
//! The byte-level recogniser is intentionally conservative: any
//! whitespace shape it doesn't expect, any token shape outside the
//! whitelist, and any structural surprise (multiple `#main`, no
//! `#main`, leading comments, decorators, `#import`, …) flips back
//! to the slow path. We never want a fast-path success that disagrees
//! with the slow path's analyzer-side judgement.

use std::sync::Arc;

use crate::lower::range_from_offsets;
use crate::token::{
    Directive, DirectiveBody, DirectiveMainParam, Expr, Node, NodeId, Operator, TypeNode,
};

/// Try the cold-start fast path on `source`. Returns `Some(Node)` when
/// the source fits the trivial-scalar `#main` envelope and a minimal
/// `Node` was built; `None` for every other shape — caller should fall
/// back to [`crate::parse_document`].
///
/// Note on semantics: the returned `Node` is the same shape (`expr` +
/// `directives` + `range` + `doc_comment = None`) the slow path
/// produces for these inputs; the parameter / type / directive
/// ranges match the byte offsets the slow path would emit, so the
/// resulting `Node` round-trips through the analyzer and tree-walker
/// without any divergence vs. `parse_document`.
pub fn parse_document_fast(source: &str) -> Option<Node> {
    let mut p = FastParser::new(source);
    p.skip_ws()?;
    // Require an immediate `#main(`. Anything else (leading comments,
    // decorators, other directives) flips back to the slow path.
    if !p.eat_str("#main") {
        return None;
    }
    p.skip_inline_ws();
    if !p.eat_char(b'(') {
        return None;
    }
    // After `skip_ws`, the cursor sits at `#main`; the open-paren just
    // consumed brings us one byte past `(`. Record the directive's
    // start offset so the directive `range` matches the slow path's.
    let directive_start_offset = p.pos - "#main(".len();
    let params = p.parse_main_params()?;
    if !p.eat_char(b')') {
        return None;
    }
    p.skip_inline_ws();
    let mut directive_end_offset = p.pos; // end of `)`
    let return_type = if p.peek_str("->") {
        p.pos += 2;
        p.skip_inline_ws();
        let t = p.parse_scalar_type()?;
        directive_end_offset = p.pos; // end of return type
        Some(t)
    } else {
        None
    };
    // The directive body ends at the return-type end (slow path
    // convention: directive range stops at the last meaningful token,
    // not the inter-token newline). Require at least one newline
    // before the body expression.
    p.skip_inline_ws();
    if !p.eat_newline() {
        return None;
    }
    p.skip_ws()?;
    let body_start = p.pos;
    let body_expr = p.parse_trivial_expr()?;
    // After the body, only trailing whitespace / newlines may remain.
    p.skip_trailing()?;
    let body_end = p.pos_after_last_token;

    let directive_range = range_from_offsets(source, directive_start_offset, directive_end_offset);
    let body_range = range_from_offsets(source, body_start, body_end);
    let doc_range = range_from_offsets(source, directive_start_offset, body_end);
    let directive = Directive {
        name: "main".to_string(),
        body: DirectiveBody::Main {
            params,
            return_type,
        },
        range: directive_range,
    };
    Some(Node {
        id: NodeId::alloc(),
        expr: Arc::new(body_expr),
        decorators: Vec::new(),
        directives: vec![directive],
        type_hint: None,
        range: doc_range,
        doc_comment: None,
    })
    .filter(|_| {
        // Final guard: body must be entirely within the source range
        // — defensive belt-and-braces against an off-by-one in the
        // recogniser leaking out as a malformed `range`.
        body_range.end.offset <= source.len()
    })
}

// ---------------------------------------------------------------------
// Internal recogniser.
// ---------------------------------------------------------------------

struct FastParser<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    /// End offset of the last token consumed by the body parser. Used
    /// when computing the document range so trailing whitespace
    /// doesn't widen the `Node.range`.
    pos_after_last_token: usize,
}

impl<'a> FastParser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            pos_after_last_token: 0,
        }
    }

    /// Skip whitespace and reject any non-whitespace pre-`#main` byte.
    /// Comments (`//` or `/* */`) flip back to the slow path so the
    /// `doc_comment` field stays correct — the slow path knows how
    /// to attach those.
    fn skip_ws(&mut self) -> Option<()> {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else if b == b'/'
                && self.pos + 1 < self.bytes.len()
                && (self.bytes[self.pos + 1] == b'/' || self.bytes[self.pos + 1] == b'*')
            {
                // Leading comments are doc-comment territory — bail.
                return None;
            } else {
                break;
            }
        }
        Some(())
    }

    fn skip_inline_ws(&mut self) {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b' ' || b == b'\t' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Skip whitespace + newlines AFTER the body. Returns `None` if
    /// any non-whitespace remains (trailing junk).
    fn skip_trailing(&mut self) -> Option<()> {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                return None;
            }
        }
        Some(())
    }

    fn eat_str(&mut self, s: &str) -> bool {
        if self.bytes.len() - self.pos >= s.len()
            && &self.bytes[self.pos..self.pos + s.len()] == s.as_bytes()
        {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn peek_str(&self, s: &str) -> bool {
        self.bytes.len() - self.pos >= s.len()
            && &self.bytes[self.pos..self.pos + s.len()] == s.as_bytes()
    }

    fn eat_char(&mut self, c: u8) -> bool {
        if self.pos < self.bytes.len() && self.bytes[self.pos] == c {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Eat a single `\n` (or `\r\n`). Returns true on success.
    fn eat_newline(&mut self) -> bool {
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\r' {
            self.pos += 1;
            if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\n' {
                self.pos += 1;
            }
            true
        } else if self.pos < self.bytes.len() && self.bytes[self.pos] == b'\n' {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_main_params(&mut self) -> Option<Vec<DirectiveMainParam>> {
        let mut params = Vec::new();
        self.skip_inline_ws();
        // Allow zero parameters: `#main() -> Int` is legal but the
        // analyzer's main-signature pass would reject a zero-param entry
        // for any host-pushed args. The fast path mirrors the slow
        // path's grammar here.
        if self.peek_str(")") {
            return Some(params);
        }
        loop {
            self.skip_inline_ws();
            let type_node = self.parse_scalar_type()?;
            self.skip_inline_ws();
            let name_start = self.pos;
            let name = self.parse_identifier()?;
            let name_end = self.pos;
            let name_range = range_from_offsets(self.source, name_start, name_end);
            params.push(DirectiveMainParam {
                name,
                name_range,
                type_node,
            });
            self.skip_inline_ws();
            if self.peek_str(",") {
                self.pos += 1;
                continue;
            } else {
                break;
            }
        }
        Some(params)
    }

    /// Recognise one of `Int` / `Float` / `Bool` / `String`.
    /// No generics, no `?`, no dotted path. Anything else flips back.
    fn parse_scalar_type(&mut self) -> Option<TypeNode> {
        let start = self.pos;
        let name = self.parse_identifier()?;
        if !matches!(name.as_str(), "Int" | "Float" | "Bool" | "String") {
            return None;
        }
        // Reject any modifier that would push us out of the scalar
        // envelope — `?`, `<`, `.` after the name.
        if self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b'?' | b'<' | b'.') {
            return None;
        }
        let end = self.pos;
        Some(TypeNode {
            path: vec![name],
            generics: Vec::new(),
            is_optional: false,
            range: range_from_offsets(self.source, start, end),
            variant_fields: None,
            doc_comment: None,
        })
    }

    /// `[A-Za-z_][A-Za-z0-9_]*` — ASCII only. The trivial-main
    /// envelope never references Unicode identifiers.
    fn parse_identifier(&mut self) -> Option<String> {
        let start = self.pos;
        if start >= self.bytes.len() {
            return None;
        }
        let first = self.bytes[start];
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return None;
        }
        self.pos += 1;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        Some(self.source[start..self.pos].to_string())
    }

    /// Recognise the body shape: ternary / binary / unary over trivial
    /// leaves. Mirrors `is_trivial_body` so anything we accept here
    /// the classifier downstream also accepts. Implemented as a
    /// Pratt-style expression parser limited to whitelist operators.
    fn parse_trivial_expr(&mut self) -> Option<Expr> {
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Option<Expr> {
        let start = self.pos;
        let cond_expr = self.parse_binary(0)?;
        let cond_end = self.pos_after_last_token;
        self.skip_inline_ws();
        if self.peek_str("?") && !self.peek_str("??") {
            self.pos += 1;
            self.skip_inline_ws();
            let then_start = self.pos;
            let then_expr = self.parse_binary(0)?;
            let then_end = self.pos_after_last_token;
            self.skip_inline_ws();
            if !self.eat_char(b':') {
                return None;
            }
            self.skip_inline_ws();
            let els_start = self.pos;
            let els_expr = self.parse_binary(0)?;
            let els_end = self.pos_after_last_token;
            Some(Expr::Ternary {
                cond: Node {
                    id: NodeId::alloc(),
                    expr: Arc::new(cond_expr),
                    decorators: Vec::new(),
                    directives: Vec::new(),
                    type_hint: None,
                    range: range_from_offsets(self.source, start, cond_end),
                    doc_comment: None,
                },
                then: Node {
                    id: NodeId::alloc(),
                    expr: Arc::new(then_expr),
                    decorators: Vec::new(),
                    directives: Vec::new(),
                    type_hint: None,
                    range: range_from_offsets(self.source, then_start, then_end),
                    doc_comment: None,
                },
                els: Node {
                    id: NodeId::alloc(),
                    expr: Arc::new(els_expr),
                    decorators: Vec::new(),
                    directives: Vec::new(),
                    type_hint: None,
                    range: range_from_offsets(self.source, els_start, els_end),
                    doc_comment: None,
                },
            })
        } else {
            Some(cond_expr)
        }
    }

    fn parse_binary(&mut self, min_prec: u8) -> Option<Expr> {
        let lhs_start = self.pos;
        let mut lhs = self.parse_unary()?;
        let mut lhs_end = self.pos_after_last_token;
        loop {
            self.skip_inline_ws();
            let Some((op, prec)) = self.peek_binary_op() else {
                break;
            };
            if prec < min_prec {
                break;
            }
            // Advance past the operator token.
            let op_len = op_str(op).len();
            self.pos += op_len;
            self.skip_inline_ws();
            let rhs_start = self.pos;
            let rhs = self.parse_binary(prec + 1)?;
            let rhs_end = self.pos_after_last_token;
            let lhs_node = Node {
                id: NodeId::alloc(),
                expr: Arc::new(lhs),
                decorators: Vec::new(),
                directives: Vec::new(),
                type_hint: None,
                range: range_from_offsets(self.source, lhs_start, lhs_end),
                doc_comment: None,
            };
            let rhs_node = Node {
                id: NodeId::alloc(),
                expr: Arc::new(rhs),
                decorators: Vec::new(),
                directives: Vec::new(),
                type_hint: None,
                range: range_from_offsets(self.source, rhs_start, rhs_end),
                doc_comment: None,
            };
            lhs = Expr::Binary(op, lhs_node, rhs_node);
            lhs_end = rhs_end;
            // After folding lhs/rhs, the new lhs range spans
            // [lhs_start, rhs_end) on the outer iteration.
            // `pos_after_last_token` keeps tracking the rightmost token.
        }
        // Outer caller's caller may need to know where this expression ended.
        self.pos_after_last_token = lhs_end;
        Some(lhs)
    }

    fn parse_unary(&mut self) -> Option<Expr> {
        self.skip_inline_ws();
        // Whitelist: leading `!`, `-`. `+` is not accepted (parses as
        // explicit positive sign which the slow path also rejects on
        // the trivial leaf level).
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'!' {
            self.pos += 1;
            self.skip_inline_ws();
            let inner_start = self.pos;
            let inner = self.parse_unary()?;
            let inner_end = self.pos_after_last_token;
            return Some(Expr::Unary(
                Operator::Not,
                Node {
                    id: NodeId::alloc(),
                    expr: Arc::new(inner),
                    decorators: Vec::new(),
                    directives: Vec::new(),
                    type_hint: None,
                    range: range_from_offsets(self.source, inner_start, inner_end),
                    doc_comment: None,
                },
            ));
        }
        // Bare `-x` is parsed as `Binary(Sub, ...)`'s rhs in the slow
        // path. For the trivial-main shape the parser only accepts
        // negative numbers as part of `Int` literals — keep parity by
        // letting `parse_leaf` handle the leading `-` on numbers.
        self.parse_leaf()
    }

    fn parse_leaf(&mut self) -> Option<Expr> {
        self.skip_inline_ws();
        let start = self.pos;
        if start >= self.bytes.len() {
            return None;
        }
        let b = self.bytes[start];
        // Negative numeric literals (`-1`) are intentionally NOT
        // recognised here: the slow path lowers them as
        // `Unary(Sub, Int)` whose `range` excludes the leading `-`,
        // so a fast-path `Int(-1)` would diverge from the slow path's
        // legacy `Node` shape. Bail and let the slow path handle.
        if b == b'-' {
            return None;
        }
        if b.is_ascii_digit() {
            return self.parse_number(start);
        }
        // Boolean literals + identifier.
        if b.is_ascii_alphabetic() || b == b'_' {
            let name = self.parse_identifier()?;
            self.pos_after_last_token = self.pos;
            return Some(match name.as_str() {
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                "null" => return None,
                _ => {
                    // Single-segment Variable. Reject if a `.` / `(` /
                    // `[` follows — those would be paths / fn calls /
                    // index reads, all outside the envelope.
                    if self.pos < self.bytes.len()
                        && matches!(self.bytes[self.pos], b'.' | b'(' | b'[')
                    {
                        return None;
                    }
                    let name_range = range_from_offsets(self.source, start, self.pos);
                    Expr::Variable(vec![crate::token::TokenKey::String(
                        name, name_range, false,
                    )])
                }
            });
        }
        // String literal — short / unescaped only. Anything fancy
        // (`f"..."`, escapes, multi-line) flips back to the slow path.
        if b == b'"' {
            self.pos += 1;
            let content_start = self.pos;
            while self.pos < self.bytes.len() {
                let c = self.bytes[self.pos];
                if c == b'\\' || c == b'\n' || c == b'\r' {
                    return None;
                }
                if c == b'"' {
                    let s = self.source[content_start..self.pos].to_string();
                    self.pos += 1;
                    self.pos_after_last_token = self.pos;
                    return Some(Expr::String(s));
                }
                self.pos += 1;
            }
            return None;
        }
        // Parenthesised sub-expressions are NOT recognised on the
        // fast path: the slow path's lowering keeps the *inner*
        // expression range tight (excludes the parens), so a naive
        // fast-path implementation would diverge. The W11 envelope
        // and the trivial-main classifier accept paren-less arithmetic
        // already; bail to the slow path for the rare parenthesised
        // shape.
        if b == b'(' {
            return None;
        }
        None
    }

    fn parse_number(&mut self, start: usize) -> Option<Expr> {
        // Consume integer / fractional digits + optional exponent.
        let mut saw_dot = false;
        let mut saw_exp = false;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c.is_ascii_digit() {
                self.pos += 1;
            } else if c == b'.' && !saw_dot && !saw_exp {
                // Look ahead — `1.foo` is a path, `1.0` is a float.
                if self.pos + 1 < self.bytes.len() && self.bytes[self.pos + 1].is_ascii_digit() {
                    saw_dot = true;
                    self.pos += 1;
                } else {
                    break;
                }
            } else if (c == b'e' || c == b'E') && !saw_exp {
                saw_exp = true;
                self.pos += 1;
                if self.pos < self.bytes.len()
                    && (self.bytes[self.pos] == b'+' || self.bytes[self.pos] == b'-')
                {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
        let text = &self.source[start..self.pos];
        self.pos_after_last_token = self.pos;
        if saw_dot || saw_exp {
            let v: f64 = text.parse().ok()?;
            Some(Expr::Float(ordered_float::OrderedFloat(v)))
        } else {
            let v: i64 = text.parse().ok()?;
            Some(Expr::Int(v))
        }
    }

    /// Pratt-style peek: returns `(operator, precedence)` if the next
    /// non-whitespace token starts a whitelisted binary operator;
    /// `None` otherwise. Precedence levels mirror the slow path's
    /// Pratt table relative ordering for the operators we accept.
    fn peek_binary_op(&self) -> Option<(Operator, u8)> {
        if self.pos >= self.bytes.len() {
            return None;
        }
        let b = self.bytes[self.pos];
        // Two-char operators first.
        if self.peek_str("==") {
            return Some((Operator::Eq, 4));
        }
        if self.peek_str("!=") {
            return Some((Operator::Ne, 4));
        }
        if self.peek_str("<=") {
            return Some((Operator::Le, 5));
        }
        if self.peek_str(">=") {
            return Some((Operator::Ge, 5));
        }
        // Single-char operators. Reject `<` and `>` when followed by
        // a `=` because we already matched the two-char form above.
        match b {
            b'+' => Some((Operator::Add, 6)),
            b'-' => {
                // `-` is binary subtraction here; the leading-`-` unary
                // case is handled inside `parse_leaf`'s number path.
                Some((Operator::Sub, 6))
            }
            b'*' => Some((Operator::Mul, 7)),
            b'/' => {
                // Reject `//` or `/*` (comments).
                if self.pos + 1 < self.bytes.len()
                    && (self.bytes[self.pos + 1] == b'/' || self.bytes[self.pos + 1] == b'*')
                {
                    None
                } else {
                    Some((Operator::Div, 7))
                }
            }
            b'%' => Some((Operator::Mod, 7)),
            b'<' => Some((Operator::Lt, 5)),
            b'>' => Some((Operator::Gt, 5)),
            _ => None,
        }
    }
}

fn op_str(op: Operator) -> &'static str {
    match op {
        Operator::Add => "+",
        Operator::Sub => "-",
        Operator::Mul => "*",
        Operator::Div => "/",
        Operator::Mod => "%",
        Operator::Eq => "==",
        Operator::Ne => "!=",
        Operator::Lt => "<",
        Operator::Gt => ">",
        Operator::Le => "<=",
        Operator::Ge => ">=",
        Operator::And | Operator::Or | Operator::Not | Operator::Pipe | Operator::Concat => {
            // Not reachable from the fast-path operator whitelist —
            // included here so the match is total.
            ""
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_document;

    fn assert_eq_modulo_ids(a: &Node, b: &Node) {
        // Compare structural shape; PartialEq on Node intentionally
        // excludes `id`, so this is equivalent to `a == b` once we
        // also normalise ranges (the fast path uses byte-level
        // offsets, the slow path uses CST-derived ones — they should
        // match because we hand the same source).
        assert_eq!(a, b, "fast vs slow path Node mismatch");
    }

    #[test]
    fn fast_path_matches_slow_path_on_w11_shape() {
        let src = "#main(Int x) -> Int\nx + 1\n";
        let fast = parse_document_fast(src).expect("fast path must accept");
        let slow = parse_document(src).expect("slow path must accept");
        assert_eq_modulo_ids(&fast, &slow);
    }

    #[test]
    fn fast_path_matches_slow_path_on_int_literal_body() {
        let src = "#main(Int x) -> Int\n42\n";
        let fast = parse_document_fast(src).expect("fast path must accept");
        let slow = parse_document(src).expect("slow path must accept");
        assert_eq_modulo_ids(&fast, &slow);
    }

    #[test]
    fn fast_path_matches_slow_path_on_multi_param() {
        let src = "#main(Int x, Int y) -> Int\nx * y + 7\n";
        let fast = parse_document_fast(src).expect("fast path must accept");
        let slow = parse_document(src).expect("slow path must accept");
        assert_eq_modulo_ids(&fast, &slow);
    }

    #[test]
    fn fast_path_matches_slow_path_on_ternary() {
        let src = "#main(Int x) -> Int\nx > 0 ? x : 0\n";
        let fast = parse_document_fast(src).expect("fast path must accept");
        let slow = parse_document(src).expect("slow path must accept");
        assert_eq_modulo_ids(&fast, &slow);
    }

    #[test]
    fn fast_path_rejects_leading_comment() {
        let src = "// hello\n#main(Int x) -> Int\nx + 1\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_rejects_decorator() {
        let src = "@brand(X)\n#main(Int x) -> Int\nx + 1\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_rejects_import_directive() {
        let src = "#import std from \"std/string\"\n#main(Int x) -> Int\nx + 1\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_rejects_list_body() {
        let src = "#main(Int x) -> Int\n[1, 2, 3]\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_rejects_fn_call_body() {
        let src = "#main(Int x) -> Int\nabs(x)\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_rejects_generic_param_type() {
        let src = "#main(List<Int> xs) -> Int\n0\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_rejects_optional_param_type() {
        let src = "#main(Int? x) -> Int\n0\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_rejects_trailing_garbage() {
        let src = "#main(Int x) -> Int\nx + 1\nextra\n";
        assert!(parse_document_fast(src).is_none());
    }

    #[test]
    fn fast_path_matches_slow_path_with_no_return_type() {
        let src = "#main(Int x)\nx + 1\n";
        let fast = parse_document_fast(src).expect("fast path must accept");
        let slow = parse_document(src).expect("slow path must accept");
        assert_eq_modulo_ids(&fast, &slow);
    }

    #[test]
    fn fast_path_matches_slow_path_on_string_literal_body() {
        let src = "#main(String s) -> String\n\"hello\"\n";
        let fast = parse_document_fast(src).expect("fast path must accept");
        let slow = parse_document(src).expect("slow path must accept");
        assert_eq_modulo_ids(&fast, &slow);
    }

    #[test]
    fn fast_path_bails_on_negative_number_literal() {
        // Slow path lowers `-1` as `Unary(Sub, Int(1))` whose range
        // excludes the leading `-`. A naive fast-path `Int(-1)` would
        // diverge; bail so the slow path can produce the canonical
        // shape.
        let src = "#main(Int x) -> Int\n-1\n";
        assert!(parse_document_fast(src).is_none());
        // The slow path must still accept this so the analyzer sees
        // it (caller falls back).
        assert!(parse_document(src).is_ok());
    }

    #[test]
    fn fast_path_bails_on_parenthesised_subexpression() {
        // Slow path keeps the inner-expression range tight (excludes
        // surrounding parens). Fast path bails so the slow path
        // produces the canonical shape.
        let src = "#main(Int x) -> Int\n(x + 1) * 2\n";
        assert!(parse_document_fast(src).is_none());
        assert!(parse_document(src).is_ok());
    }
}
