//! Concrete syntax tree (CST) builder over the lossless [`lex`]
//! output. P2 of the rowan rewrite — translates the existing winnow
//! grammar into rowan `GreenNode`s while preserving every source byte
//! (including whitespace and comments) as first-class tokens.
//!
//! Architecture
//! ============
//!
//! - `Parser` wraps the flat `(SyntaxKind, &str)` token stream from
//!   [`lex::lex`] plus a `rowan::GreenNodeBuilder` writing the tree.
//! - "Skip-trivia" helpers (`current`, `at`, `nth`) ignore whitespace
//!   and comments, so productions can pattern-match on meaningful
//!   structure without ever forgetting to write a trivia token to the
//!   tree.
//! - Trivia is flushed to the builder lazily — emitted as siblings
//!   *just before* the next meaningful token. The "right" home for a
//!   trailing comment (does it belong to the closing brace, or to the
//!   next pair?) is decided by `bump`'s flush order.
//! - Each grammar production is a function on `&mut Parser`. They
//!   call `open(kind)` / `close()` to mark composite nodes. Failures
//!   recover via `error_recover(sync_set)` which emits an ERROR node
//!   and synchronises to the nearest token in `sync_set`.
//!
//! Scope
//! =====
//!
//! P2 covers the structural surface: literals, identifiers, paths,
//! references, lists, dicts (with pair attributes), unary / binary /
//! ternary expressions, calls, closures. Higher-level constructs
//! (`#schema`, `#main` signatures, `match`, `where`, comprehensions)
//! and error-recovery refinement land in subsequent commits — the
//! `ERROR` node already keeps the round-trip invariant honest for
//! unimplemented cases.

use crate::lex;
use crate::syntax::{RelonLanguage, SyntaxKind, SyntaxNode};
use rowan::{Checkpoint, GreenNodeBuilder};

/// One parse failure with an attached byte position. Always reachable
/// from the resulting CST through the spanning `ERROR` node, but
/// surfacing them separately gives callers (LSP diagnostics, CLI
/// pretty-printer) a flat list without re-walking the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// Byte offset into the original source where recovery began.
    pub offset: usize,
}

/// Successful parse result. `green` is the lossless tree; `errors`
/// is the (possibly empty) list of parse errors emitted along the
/// way. The parser NEVER returns `Err` — every input shape produces
/// a tree, with `ERROR` nodes covering unparseable spans.
#[derive(Debug, Clone)]
pub struct Parse {
    green: rowan::GreenNode,
    pub errors: Vec<ParseError>,
}

impl Parse {
    /// Wrap the green tree as a typed [`SyntaxNode`] for traversal.
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    /// Returns `true` when at least one parse error was emitted.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Top-level entry. Always produces a `Parse` — never panics, never
/// returns `Err`. Bytes that don't fit any production are absorbed
/// into `ERROR` nodes; the round-trip invariant holds regardless.
pub fn parse_cst(source: &str) -> Parse {
    let tokens = lex::lex(source);
    let mut parser = Parser::new(&tokens);
    parser.parse_document();
    parser.finish()
}

// =====================================================================
// Parser state.
// =====================================================================

struct Parser<'a> {
    tokens: &'a [(SyntaxKind, &'a str)],
    pos: usize,
    builder: GreenNodeBuilder<'static>,
    errors: Vec<ParseError>,
    /// Running byte offset — kept in sync with `pos` so we can record
    /// error positions without re-walking.
    cursor_byte: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [(SyntaxKind, &'a str)]) -> Self {
        Self {
            tokens,
            pos: 0,
            builder: GreenNodeBuilder::new(),
            errors: Vec::new(),
            cursor_byte: 0,
        }
    }

    fn finish(self) -> Parse {
        // `parse_document` is responsible for emitting every token
        // INSIDE the root DOCUMENT node — rowan requires it. The
        // `finish()` call here just hands ownership of the green
        // tree back.
        debug_assert!(
            self.pos >= self.tokens.len(),
            "{} tokens unflushed at parse end",
            self.tokens.len() - self.pos
        );
        Parse {
            green: self.builder.finish(),
            errors: self.errors,
        }
    }

    // ----- token-stream introspection ----------------------------------

    /// Kind of the next *non-trivia* token, or `None` if EOI.
    fn current(&self) -> Option<SyntaxKind> {
        self.nth(0)
    }

    /// Kind of the `n`-th non-trivia token ahead (0 = current), or
    /// `None` if there aren't that many. Useful for productions that
    /// need 1-token lookahead.
    fn nth(&self, n: usize) -> Option<SyntaxKind> {
        let mut idx = self.pos;
        let mut left = n;
        while idx < self.tokens.len() {
            let kind = self.tokens[idx].0;
            if kind.is_trivia() {
                idx += 1;
                continue;
            }
            if left == 0 {
                return Some(kind);
            }
            left -= 1;
            idx += 1;
        }
        None
    }

    fn at(&self, kind: SyntaxKind) -> bool {
        self.current() == Some(kind)
    }

    fn at_set(&self, set: &[SyntaxKind]) -> bool {
        self.current().is_some_and(|k| set.contains(&k))
    }

    fn at_end(&self) -> bool {
        self.current().is_none()
    }

    // ----- consumption --------------------------------------------------

    /// Emit any pending trivia tokens to the builder. Trivia tokens
    /// (whitespace, comments) are skipped by `current` / `at` but
    /// still need to land in the tree — this writes them flush
    /// against whatever production opened most recently.
    fn flush_trivia(&mut self) {
        while self.pos < self.tokens.len() {
            let (kind, text) = self.tokens[self.pos];
            if !kind.is_trivia() {
                return;
            }
            self.builder
                .token(RelonLanguage::kind_to_raw_static(kind), text);
            self.cursor_byte += text.len();
            self.pos += 1;
        }
    }

    /// Consume the next non-trivia token and emit it to the builder,
    /// preceded by any pending trivia. Panics in tests if called at
    /// EOI — productions should guard with `current()` first.
    fn bump(&mut self) {
        self.flush_trivia();
        if self.pos >= self.tokens.len() {
            debug_assert!(false, "bump() past end of input");
            return;
        }
        let (kind, text) = self.tokens[self.pos];
        self.builder
            .token(RelonLanguage::kind_to_raw_static(kind), text);
        self.cursor_byte += text.len();
        self.pos += 1;
    }

    /// Consume the next non-trivia token if it matches `kind`.
    /// Returns `true` on consume.
    fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Consume `kind` or emit a parse error. Returns `true` on
    /// success; on failure leaves the cursor where it was and pushes
    /// to `errors`. Productions that need to keep going should follow
    /// `expect` with `error_recover` for proper sync behaviour.
    fn expect(&mut self, kind: SyntaxKind) -> bool {
        if self.eat(kind) {
            true
        } else {
            self.error(format!("expected {kind:?}, found {:?}", self.current()));
            false
        }
    }

    fn error(&mut self, message: impl Into<String>) {
        self.errors.push(ParseError {
            message: message.into(),
            offset: self.cursor_byte,
        });
    }

    /// Wrap the next token (or a synthetic empty span) in an `ERROR`
    /// node and push an error. Used as a one-shot way to mark an
    /// unexpected leaf without entering recovery.
    fn error_at_current(&mut self, message: impl Into<String>) {
        self.error(message);
        self.open(SyntaxKind::ERROR);
        if !self.at_end() {
            self.bump();
        }
        self.close();
    }

    /// Emit an `ERROR` node spanning every token until one of
    /// `sync_set` is reached (or EOI). The error message is recorded
    /// at the offset where recovery started.
    fn error_recover(&mut self, message: impl Into<String>, sync_set: &[SyntaxKind]) {
        self.error(message);
        self.open(SyntaxKind::ERROR);
        while !self.at_end() && !self.at_set(sync_set) {
            self.bump();
        }
        self.close();
    }

    // ----- node bracketing ---------------------------------------------

    fn open(&mut self, kind: SyntaxKind) {
        // Order matters: `start_node` MUST come before `flush_trivia`
        // so any pending whitespace / comments land INSIDE the new
        // node (as leading trivia of its first child) rather than as
        // siblings of the node at the parent level. Flushing first
        // would also break the very-first `open(DOCUMENT)` call —
        // leading file trivia would end up at rowan's root level,
        // violating the "exactly one root" invariant.
        self.builder
            .start_node(RelonLanguage::kind_to_raw_static(kind));
        self.flush_trivia();
    }

    fn checkpoint(&mut self) -> Checkpoint {
        // Checkpoint snaps to "right after any pending trivia" —
        // `open_at(ck, ..)` wraps the construct that follows, NOT
        // the trivia in front of it. Otherwise a comment before a
        // binary expression would get pulled inside the
        // `BINARY_EXPR` node, which is the wrong attachment.
        self.flush_trivia();
        self.builder.checkpoint()
    }

    fn open_at(&mut self, ck: Checkpoint, kind: SyntaxKind) {
        self.builder
            .start_node_at(ck, RelonLanguage::kind_to_raw_static(kind));
    }

    fn close(&mut self) {
        self.builder.finish_node();
    }

    // =================================================================
    // Productions.
    // =================================================================

    /// Top-level: zero-or-more attributes, then one document value.
    /// The whole thing is wrapped in a `DOCUMENT` node so the round
    /// trip walks from a single root.
    fn parse_document(&mut self) {
        self.open(SyntaxKind::DOCUMENT);
        // Leading directives / decorators stacked above the root
        // value. The grammar permits them at file scope (e.g.
        // `#schema X { ... }` files with no separate value body).
        while self.at(SyntaxKind::HASH) || self.at(SyntaxKind::AT) {
            self.parse_attribute();
        }
        // The root value. EOI is fine — files like
        // `#schema X { ... }` end after the directive's body.
        if !self.at_end() {
            self.parse_expr();
        }
        // Anything left over is unexpected trailing input — wrap as
        // ERROR so the round-trip stays whole.
        if !self.at_end() {
            self.error_recover("trailing input after root value", &[]);
        }
        // Trailing trivia (final newline, footer comments) MUST land
        // inside DOCUMENT — rowan only accepts one root node, and
        // tokens emitted after `close()` would have nowhere to live.
        self.flush_trivia();
        self.close();
    }

    /// `@name(...)` or `#name <body>`. Lightweight v1 — captures the
    /// attribute body as a generic expression, lets the typed-AST
    /// layer (P3) lower it to `Directive` / `Decorator`. Recovery
    /// resyncs to the next attribute or the start of a value.
    fn parse_attribute(&mut self) {
        let kind = if self.at(SyntaxKind::HASH) {
            SyntaxKind::DIRECTIVE
        } else {
            SyntaxKind::DECORATOR
        };
        self.open(kind);
        self.bump(); // # or @
        if self.at(SyntaxKind::IDENT) {
            self.bump();
        } else {
            self.error_at_current("expected attribute name");
        }
        // Optional body — a `(...)` arg list for decorators, or a
        // free-form expression for directives. We delegate to the
        // primary parser; it'll stop at the next attribute / EOI.
        // Conservative for v1: only consume a single primary token
        // sequence if the next thing isn't another attribute or the
        // root value boundary.
        if self.at(SyntaxKind::L_PAREN) {
            self.parse_call_args();
        } else if self.is_attribute_body_start() {
            self.parse_expr();
        }
        self.close();
    }

    fn is_attribute_body_start(&self) -> bool {
        self.current().is_some_and(|k| {
            matches!(
                k,
                SyntaxKind::IDENT
                    | SyntaxKind::NUMBER
                    | SyntaxKind::STRING
                    | SyntaxKind::L_BRACE
                    | SyntaxKind::L_BRACK
                    | SyntaxKind::AMP
                    | SyntaxKind::MINUS
                    | SyntaxKind::BANG
                    | SyntaxKind::STAR
            )
        })
    }

    // ----- expression entry -------------------------------------------

    /// Parse a full expression. Operator precedence is climbed with a
    /// Pratt-style loop. Lowest precedence first; primary handles
    /// atoms and prefix unaries.
    fn parse_expr(&mut self) {
        self.parse_expr_bp(0);
    }

    fn parse_expr_bp(&mut self, min_bp: u8) {
        let lhs_ck = self.checkpoint();
        self.parse_unary();

        loop {
            let Some(op) = self.current() else { break };
            let Some((lbp, rbp)) = infix_bp(op) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            self.open_at(lhs_ck, SyntaxKind::BINARY_EXPR);
            self.bump();
            self.parse_expr_bp(rbp);
            self.close();
        }
    }

    /// Prefix-unary or atom. Postfix call / index / dot are wrapped
    /// here via checkpoint.
    fn parse_unary(&mut self) {
        if self.at_set(&[SyntaxKind::MINUS, SyntaxKind::BANG, SyntaxKind::PLUS]) {
            self.open(SyntaxKind::UNARY_EXPR);
            self.bump();
            self.parse_unary();
            self.close();
            return;
        }
        self.parse_postfix();
    }

    /// Atom with postfix suffixes (`.field`, `[i]`, `(args)`).
    fn parse_postfix(&mut self) {
        let ck = self.checkpoint();
        self.parse_atom();
        loop {
            if self.at(SyntaxKind::L_PAREN) {
                self.open_at(ck, SyntaxKind::CALL_EXPR);
                self.parse_call_args();
                self.close();
            } else if self.at(SyntaxKind::DOT) || self.at(SyntaxKind::L_BRACK) {
                // Path access — fold into VARIABLE_EXPR so dotted
                // paths like `a.b.c` end up as a single node.
                self.open_at(ck, SyntaxKind::VARIABLE_EXPR);
                while self.at(SyntaxKind::DOT) || self.at(SyntaxKind::L_BRACK) {
                    if self.at(SyntaxKind::DOT) {
                        self.bump();
                        if self.at(SyntaxKind::IDENT) {
                            self.bump();
                        } else {
                            self.error_at_current("expected identifier after `.`");
                        }
                    } else {
                        // `[ index ]`
                        self.bump();
                        self.parse_expr();
                        self.expect(SyntaxKind::R_BRACK);
                    }
                }
                self.close();
            } else {
                break;
            }
        }
    }

    fn parse_atom(&mut self) {
        match self.current() {
            Some(SyntaxKind::NUMBER) | Some(SyntaxKind::STRING) => {
                self.open(SyntaxKind::LITERAL);
                self.bump();
                self.close();
            }
            Some(SyntaxKind::IDENT) => {
                // `null` / `true` / `false` are keyword-shaped
                // literals but lex as IDENT — promote here.
                let text = self.tokens[self.pos_skip_trivia()].1;
                if matches!(text, "null" | "true" | "false") {
                    self.open(SyntaxKind::LITERAL);
                    self.bump();
                    self.close();
                } else {
                    self.open(SyntaxKind::VARIABLE_EXPR);
                    self.bump();
                    self.close();
                }
            }
            Some(SyntaxKind::AMP) => self.parse_reference(),
            Some(SyntaxKind::L_BRACE) => self.parse_dict(),
            Some(SyntaxKind::L_BRACK) => self.parse_list(),
            Some(SyntaxKind::L_PAREN) => {
                // Parenthesised expression (P2 has no closure syntax
                // detection yet — added with closures production).
                self.bump();
                self.parse_expr();
                self.expect(SyntaxKind::R_PAREN);
            }
            Some(SyntaxKind::STAR) => {
                self.open(SyntaxKind::WILDCARD);
                self.bump();
                self.close();
            }
            Some(SyntaxKind::ELLIPSIS) => {
                self.open(SyntaxKind::SPREAD_EXPR);
                self.bump();
                self.parse_unary();
                self.close();
            }
            _ => self.error_at_current("expected expression"),
        }
    }

    /// Index into `tokens` of the next non-trivia token. Caller must
    /// guarantee `current().is_some()`.
    fn pos_skip_trivia(&self) -> usize {
        let mut idx = self.pos;
        while idx < self.tokens.len() && self.tokens[idx].0.is_trivia() {
            idx += 1;
        }
        idx
    }

    fn parse_reference(&mut self) {
        // `&base.tail.tail...`
        self.open(SyntaxKind::REFERENCE_EXPR);
        self.bump(); // &
        if self.at(SyntaxKind::IDENT) {
            self.bump(); // base name
        } else {
            self.error_at_current("expected reference base after `&`");
        }
        while self.at(SyntaxKind::DOT) || self.at(SyntaxKind::L_BRACK) {
            if self.at(SyntaxKind::DOT) {
                self.bump();
                if self.at(SyntaxKind::IDENT) {
                    self.bump();
                } else {
                    self.error_at_current("expected identifier after `.`");
                }
            } else {
                self.bump(); // [
                self.parse_expr();
                self.expect(SyntaxKind::R_BRACK);
            }
        }
        self.close();
    }

    fn parse_list(&mut self) {
        self.open(SyntaxKind::LIST);
        self.bump(); // [
        while !self.at(SyntaxKind::R_BRACK) && !self.at_end() {
            self.parse_expr();
            if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_BRACK) {
                self.error_recover(
                    "expected `,` or `]` in list",
                    &[SyntaxKind::COMMA, SyntaxKind::R_BRACK],
                );
                // Consume the comma if recovery landed on it.
                self.eat(SyntaxKind::COMMA);
            }
        }
        self.expect(SyntaxKind::R_BRACK);
        self.close();
    }

    fn parse_dict(&mut self) {
        self.open(SyntaxKind::DICT);
        self.bump(); // {
        while !self.at(SyntaxKind::R_BRACE) && !self.at_end() {
            self.parse_dict_field();
            if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_BRACE) {
                self.error_recover(
                    "expected `,` or `}` in dict",
                    &[SyntaxKind::COMMA, SyntaxKind::R_BRACE],
                );
                self.eat(SyntaxKind::COMMA);
            }
        }
        self.expect(SyntaxKind::R_BRACE);
        self.close();
    }

    fn parse_dict_field(&mut self) {
        self.open(SyntaxKind::DICT_FIELD);
        // Leading attributes (e.g. `#private` / `#expect "msg"` /
        // `@currency("USD")`) stack above the pair's key. Same
        // shape the file root permits.
        while self.at(SyntaxKind::HASH) || self.at(SyntaxKind::AT) {
            self.parse_attribute();
        }
        if self.at_end() {
            self.close();
            return;
        }
        // The key: an ident, a string, or `...` (spread).
        if self.at(SyntaxKind::ELLIPSIS) {
            self.open(SyntaxKind::SPREAD_EXPR);
            self.bump();
            self.parse_expr();
            self.close();
            self.close();
            return;
        }
        if self.at(SyntaxKind::IDENT) || self.at(SyntaxKind::STRING) {
            self.bump();
        } else {
            self.error_recover(
                "expected dict key",
                &[
                    SyntaxKind::COLON,
                    SyntaxKind::COMMA,
                    SyntaxKind::R_BRACE,
                ],
            );
        }
        // Either `:` value, or `(params): body` (closure-typed
        // pair). v1 just consumes the `(...)` as call-args; the
        // typed layer will recognise the closure shape.
        if self.at(SyntaxKind::L_PAREN) {
            self.parse_call_args();
        }
        if self.eat(SyntaxKind::COLON) {
            self.parse_expr();
        } else {
            self.error("expected `:` in dict field");
        }
        self.close();
    }

    fn parse_call_args(&mut self) {
        self.open(SyntaxKind::CALL_ARG);
        self.bump(); // (
        while !self.at(SyntaxKind::R_PAREN) && !self.at_end() {
            self.parse_expr();
            if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_PAREN) {
                self.error_recover(
                    "expected `,` or `)` in argument list",
                    &[SyntaxKind::COMMA, SyntaxKind::R_PAREN],
                );
                self.eat(SyntaxKind::COMMA);
            }
        }
        self.expect(SyntaxKind::R_PAREN);
        self.close();
    }
}

// =====================================================================
// Operator precedence (Pratt binding-power table).
//
// Mirrors the existing precedence chain in `expr.rs`:
//   1. or   ||
//   2. and  &&
//   3. equality   ==  !=
//   4. comparison <  >  <=  >=
//   5. concat  ++
//   6. additive + -
//   7. multiplicative * / %
//   8. pipe |
// All operators are left-associative (right_bp = left_bp + 1).
// =====================================================================

fn infix_bp(kind: SyntaxKind) -> Option<(u8, u8)> {
    Some(match kind {
        SyntaxKind::PIPE_PIPE => (10, 11),
        SyntaxKind::AMP_AMP => (20, 21),
        SyntaxKind::EQ_EQ | SyntaxKind::BANG_EQ => (30, 31),
        SyntaxKind::LT | SyntaxKind::GT | SyntaxKind::LT_EQ | SyntaxKind::GT_EQ => (40, 41),
        SyntaxKind::PLUS_PLUS => (50, 51),
        SyntaxKind::PLUS | SyntaxKind::MINUS => (60, 61),
        SyntaxKind::STAR | SyntaxKind::SLASH | SyntaxKind::PERCENT => (70, 71),
        SyntaxKind::PIPE => (80, 81),
        _ => return None,
    })
}

// =====================================================================
// rowan `Language::kind_to_raw` is an instance method on a unit type;
// our hot inner loops want a `'static`-friendly free function. Wrap it.
// =====================================================================

trait RawKind {
    fn kind_to_raw_static(kind: SyntaxKind) -> rowan::SyntaxKind;
}
impl RawKind for RelonLanguage {
    fn kind_to_raw_static(kind: SyntaxKind) -> rowan::SyntaxKind {
        kind.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_round_trip(source: &str) -> Parse {
        let parsed = parse_cst(source);
        let reconstructed = parsed.syntax().text().to_string();
        assert_eq!(reconstructed, source, "round-trip mismatch");
        parsed
    }

    #[test]
    fn empty_dict() {
        let parsed = parse_round_trip("{}");
        assert!(!parsed.has_errors());
    }

    #[test]
    fn simple_dict() {
        parse_round_trip("{ foo: 1, bar: 2 }");
    }

    #[test]
    fn nested_dict_and_list() {
        parse_round_trip(
            "{\n    foo: [1, 2, 3],\n    bar: { baz: \"hi\" }\n}\n",
        );
    }

    #[test]
    fn reference_path() {
        parse_round_trip("{ x: &root.foo.bar[0] }");
    }

    #[test]
    fn binary_expression() {
        let parsed = parse_round_trip("{ x: 1 + 2 * 3 }");
        assert!(!parsed.has_errors());
        // Multiplicative inside additive — verify the BINARY_EXPR
        // nesting by looking at the syntax tree.
        let syntax = parsed.syntax();
        let dict = syntax
            .descendants()
            .find(|n| n.kind() == SyntaxKind::DICT)
            .expect("dict");
        let outer_binary = dict
            .descendants()
            .find(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .expect("outer binary");
        // The outer binary is `1 + (2 * 3)`. The right child is
        // another BINARY_EXPR.
        let inner_binaries: Vec<_> = outer_binary
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::BINARY_EXPR && *n != outer_binary)
            .collect();
        assert!(!inner_binaries.is_empty(), "expected nested BINARY_EXPR");
    }

    #[test]
    fn closure_shaped_pair_uses_call_args() {
        // v1 (P2): `name(p1, p2): body` parses as DICT_FIELD with
        // a CALL_ARG before the colon. P3 will reinterpret this as
        // a closure; for now we just verify the shape round-trips.
        parse_round_trip("{ add(a, b): a + b }");
    }

    #[test]
    fn comments_round_trip() {
        parse_round_trip(
            "// header\n{\n    // inner\n    x: 1, /* trail */ y: 2\n}\n",
        );
    }

    #[test]
    fn error_recovery_preserves_bytes() {
        // Deliberate parse failure: missing colon. The recovery
        // wraps `42` in an ERROR node and resyncs to `,`. Source
        // bytes are intact end-to-end.
        let parsed = parse_round_trip("{ foo 42, bar: 1 }");
        assert!(parsed.has_errors(), "expected an error report");
    }

    #[test]
    fn unknown_byte_does_not_crash() {
        parse_round_trip("{ x: \u{0000} 1 }");
    }

    /// The strongest invariant: every checked-in `.relon` file
    /// round-trips through the CST byte-exact. Some may still have
    /// parse errors (the v2 grammar doesn't cover every construct
    /// yet) — that's expected and tolerated. What MUST hold is the
    /// lossless tree property.
    #[test]
    fn every_fixture_round_trips_through_cst() {
        use std::fs;
        use std::path::PathBuf;

        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = crate_dir
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf();
        let mut files = Vec::new();
        walk(&workspace_root, &mut files);
        files.retain(|p| !p.to_string_lossy().contains("/target/"));
        assert!(!files.is_empty());
        for path in files {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let parsed = parse_cst(&source);
            let reconstructed = parsed.syntax().text().to_string();
            assert_eq!(
                reconstructed, source,
                "round-trip mismatch on {path:?}"
            );
        }
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(name, "target" | "node_modules" | ".git") {
                    continue;
                }
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("relon") {
                out.push(p);
            }
        }
    }
}
