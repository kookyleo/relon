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
//! P2 (now complete) covers the full surface grammar:
//!
//! * Literals, identifiers, dotted paths, references.
//! * Lists, dicts (with pair attributes + method-shorthand closures
//!   + typed keys), list comprehensions.
//! * Unary, binary (Pratt-precedence), call, postfix `.field` /
//!   `[index]`, parenthesised closure (`(p) [-> R] => body`).
//! * `expr match { ... }` and `expr where { ... }` postfix forms.
//! * F-string decomposition into `F_STRING` + `F_STRING_LITERAL`
//!   chunks + nested `F_STRING_INTERPOLATION` sub-nodes (whose
//!   children are ordinary Relon expressions).
//! * `TYPE_NODE` — dotted paths, generics, optional `?`.
//! * Directive bodies dispatched by name: `#schema`/`#extend`
//!   (name + generics + body + optional `with`), `#import`
//!   (`<spec> from "path"`), `#main(typed-params) [-> Ret]`.
//!
//! P3 lives in `crate::ast` — typed-AST wrappers on top of this
//! CST. P4 will migrate downstream crates (analyzer, evaluator,
//! fmt, wasm, lsp) onto the new wrappers.

use crate::lex;
use crate::lex::utf8_codepoint_len_for_cst as utf8_codepoint_len;
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
    let mut parser = Parser::new(tokens);
    parser.parse_document();
    parser.finish()
}

// =====================================================================
// Parser state.
// =====================================================================

struct Parser<'a> {
    /// The flat token stream the parser is currently consuming. We
    /// own the vec so f-string interpolation sub-parses can swap in
    /// a transient inner-token list without lifetime gymnastics —
    /// the inner `&str` slices still point into the original source.
    tokens: Vec<(SyntaxKind, &'a str)>,
    pos: usize,
    builder: GreenNodeBuilder<'static>,
    errors: Vec<ParseError>,
    /// Running byte offset — kept in sync with `pos` so we can record
    /// error positions without re-walking.
    cursor_byte: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<(SyntaxKind, &'a str)>) -> Self {
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

    /// `@name(...)` or `#name <body>`. Decorator bodies are always
    /// `(args)` (or absent) and decorator names may be dotted
    /// (`@ensure.int`, `@module.fn`); directive bodies branch on the
    /// name: `schema` / `extend` capture `name <T, U>? body? (with {})?`,
    /// `import` captures `<spec> from "path"`, `main` captures
    /// `( typed-params ) [-> Ret]`, the remaining names dispatch via
    /// [`directive_shape`] — bare directives consume no body so they
    /// can sit cleanly above the field they decorate, value directives
    /// take exactly one trailing expression.
    fn parse_attribute(&mut self) {
        let is_directive = self.at(SyntaxKind::HASH);
        let kind = if is_directive {
            SyntaxKind::DIRECTIVE
        } else {
            SyntaxKind::DECORATOR
        };
        self.open(kind);
        self.bump(); // # or @
        let name_text = if self.at(SyntaxKind::IDENT) {
            let text = self.current_text();
            self.bump();
            text
        } else {
            self.error_at_current("expected attribute name");
            None
        };
        if !is_directive {
            // Decorator — name may be dotted (`@ensure.at_least`).
            // Body is always `(args)` or empty.
            while self.at(SyntaxKind::DOT) {
                self.bump();
                if self.at(SyntaxKind::IDENT) {
                    self.bump();
                } else {
                    self.error_at_current("expected identifier after `.` in decorator name");
                    break;
                }
            }
            if self.at(SyntaxKind::L_PAREN) {
                self.parse_call_args();
            }
            self.close();
            return;
        }
        // Directive — dispatch on name. Unknown directive names take a
        // single optional expression body to match the legacy parser's
        // permissive fallback.
        match name_text
            .map(directive_shape)
            .unwrap_or(DirectiveShape::Value)
        {
            DirectiveShape::Bare => {
                // No body. `#private`, `#strict`, `#native`.
            }
            DirectiveShape::Value => {
                if self.is_attribute_body_start() {
                    self.parse_expr();
                }
            }
            DirectiveShape::NameBody => self.parse_directive_name_body(),
            DirectiveShape::Import => self.parse_directive_import(),
            DirectiveShape::Main => self.parse_directive_main(),
        }
        self.close();
    }

    /// `#schema Name <T, U>? body? (with { methods... })?`. The body
    /// is whatever expression follows the name + generics (typically
    /// a dict but the parser accepts any expression — the analyzer
    /// emits a diagnostic when it isn't a dict). The trailing `with`
    /// block is optional and may also follow a body-less `#schema X`
    /// declaration.
    fn parse_directive_name_body(&mut self) {
        // Optional declared name.
        if self.at(SyntaxKind::IDENT) {
            self.bump();
        } else {
            return;
        }
        // Optional generic param list `<T, U>` — bare identifiers.
        if self.at(SyntaxKind::LT) {
            self.bump();
            while !self.at(SyntaxKind::GT) && !self.at_end() {
                if self.at(SyntaxKind::IDENT) {
                    self.bump();
                } else {
                    self.error_at_current("expected generic param");
                    break;
                }
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
            }
            self.expect(SyntaxKind::GT);
        }
        // The body is everything up to (a) the next attribute, (b)
        // the `with` keyword, or (c) the dict-field separator (`:`
        // / `,` / `}` / EOI). Special-case the `with`-only shape
        // (`#schema X with { ... }`) by skipping the body when we
        // see `with` immediately.
        let saw_with = self.at(SyntaxKind::IDENT) && self.current_text() == Some("with");
        // v1 accepts an optional `:` separator between schema name and
        // body: `#schema Image: { name: String }` is equivalent to
        // `#schema Image { name: String }`. The legacy combinator chain
        // consumed the `:` as part of the directive; the CST does the
        // same so the `is_attribute_body_start` check below sees the
        // body proper. Without this, the dict-field grammar would
        // (correctly!) parse `Image:` as a malformed dict field after
        // mistaking the directive for body-less.
        if !saw_with && self.at(SyntaxKind::COLON) {
            self.bump();
        }
        if !saw_with && self.is_attribute_body_start() {
            // Guard: when the next chars are `Ident:` / `Ident,` we
            // must not consume them — they belong to a dict field
            // following `#schema X` in a `: ...` context.
            if !self.peek_attribute_terminator() {
                // Schema bodies are typically dicts (`#schema U { ... }`)
                // but the v1 grammar also accepts a type alias body
                // (`#schema Status Enum<"on", "off">`). When the body
                // looks like a bare type expression — IDENT immediately
                // followed by `<...>` — parse it as a type so the
                // string-literal generic args don't surprise the Pratt
                // expression grammar (which would treat `<` as a
                // binary comparison).
                if self.peek_is_bare_type_body() {
                    self.parse_type();
                } else {
                    self.parse_expr();
                }
            }
        }
        // Optional `with { ... }` block — a structured method list.
        // The legacy `opt_parse_with_block` (`directive.rs`) drives the
        // shape: leading pragma stack (`#derive` / `#native` /
        // `#private` / `#no_auto_derive`), then a `name<T>?(p: T,
        // ...) -> Ret (: body)?` declaration. We emit each method as
        // a SCHEMA_METHOD node so the typed-AST layer can read the
        // structure cheaply.
        if self.at(SyntaxKind::IDENT) && self.current_text() == Some("with") {
            self.bump();
            if self.at(SyntaxKind::L_BRACE) {
                self.parse_schema_with();
            }
        }
    }

    /// True when the upcoming token stream is an IDENT followed
    /// immediately (no intervening whitespace) by `<` — the type-alias
    /// body shape `Enum<"on", "off">` / `Int` / `List<T>`. Used by
    /// `parse_directive_name_body` to disambiguate the type-body shape
    /// from a regular expression body. The IDENT-and-no-`<` case
    /// (bare-type body like `#schema MyAlias String`) is also
    /// classified as "type body" — the body is a single primitive
    /// type identifier without generics.
    fn peek_is_bare_type_body(&self) -> bool {
        if !self.at(SyntaxKind::IDENT) {
            return false;
        }
        // Only commit to the type body if the IDENT is one of the
        // known type heads (`Int`, `String`, `Bool`, `List`, `Dict`,
        // `Enum`, `Any`, `Null`, `Float`) — otherwise a regular
        // expression with a leading IDENT is the safer fallback.
        let head = self.current_text().unwrap_or("");
        if !matches!(
            head,
            "Int" | "String" | "Bool" | "Float" | "Any" | "Null" | "List" | "Dict" | "Enum"
        ) {
            return false;
        }
        // For `Enum` specifically, only commit when followed by `<`
        // (with no whitespace) — bare `Enum` isn't a sensible body.
        // For other type heads, allow both `Int` (alone) and
        // `List<T>` (with generics).
        let head_idx = self.pos_skip_trivia();
        let mut idx = head_idx + 1;
        let mut had_ws = false;
        while idx < self.tokens.len() && self.tokens[idx].0.is_trivia() {
            had_ws = true;
            idx += 1;
        }
        if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::LT) && !had_ws {
            return true;
        }
        // Bare type identifier (`#schema MyAlias String`) — only
        // accept when nothing else follows on the line. We approximate
        // "nothing else" by checking the next non-trivia token isn't
        // a typical expression-continuation symbol.
        head != "Enum"
            && matches!(
                self.tokens.get(idx).map(|(k, _)| *k),
                Some(SyntaxKind::HASH) | Some(SyntaxKind::L_BRACE) | None
            )
    }

    /// `with { (pragma | method)* }` — body of a `#schema` / `#extend`
    /// directive. Lossless: every byte (whitespace, comments, leading
    /// pragmas) sits inside the [`SCHEMA_WITH`] node, with each method
    /// declaration wrapped in its own [`SCHEMA_METHOD`] child.
    fn parse_schema_with(&mut self) {
        self.open(SyntaxKind::SCHEMA_WITH);
        self.bump(); // {
        while !self.at(SyntaxKind::R_BRACE) && !self.at_end() {
            // Method declarations are introduced by either a pragma
            // (`#derive` / `#native` / `#private` / `#no_auto_derive`)
            // or directly by a method name. We greedily group leading
            // pragmas with the next method into one SCHEMA_METHOD node
            // — if no method follows (e.g. trailing schema-level
            // `#no_auto_derive`), the directives sit at the
            // SCHEMA_WITH level as siblings.
            if self.at(SyntaxKind::HASH) {
                let ck = self.checkpoint();
                let mut had_method_pragma = false;
                while self.at(SyntaxKind::HASH) {
                    let name = self.directive_name_after_hash();
                    if matches!(
                        name.as_deref(),
                        Some("derive") | Some("native") | Some("private")
                    ) {
                        had_method_pragma = true;
                    }
                    self.parse_attribute();
                }
                if self.at(SyntaxKind::IDENT) && !self.at_method_terminator() {
                    self.open_at(ck, SyntaxKind::SCHEMA_METHOD);
                    self.parse_schema_method_after_pragmas();
                    self.close();
                } else if had_method_pragma {
                    // Pragma stack without a method — surface a recovery
                    // error to mirror the legacy "stray method pragma"
                    // diagnostic but keep parsing.
                    self.error(
                        "expected method declaration after `#derive` / `#native` / `#private`",
                    );
                }
                continue;
            }
            if self.at(SyntaxKind::IDENT) {
                self.open(SyntaxKind::SCHEMA_METHOD);
                self.parse_schema_method_after_pragmas();
                self.close();
                continue;
            }
            // Unexpected token inside the with-block — recover to the
            // next likely start of a method (HASH / IDENT / R_BRACE).
            self.error_recover(
                "expected method or pragma inside `with { ... }`",
                &[SyntaxKind::HASH, SyntaxKind::IDENT, SyntaxKind::R_BRACE],
            );
        }
        self.expect(SyntaxKind::R_BRACE);
        self.close();
    }

    /// True when the upcoming non-trivia token is the with-block
    /// terminator (`}`) — used to spot a pragma stack with no method
    /// trailing it without confusing it for a normal method header.
    fn at_method_terminator(&self) -> bool {
        matches!(self.current(), Some(SyntaxKind::R_BRACE)) || self.at_end()
    }

    /// Peek the IDENT immediately after a HASH at the current position
    /// (skipping trivia). Returns `None` if `#` isn't followed by an
    /// identifier.
    fn directive_name_after_hash(&self) -> Option<String> {
        let mut idx = self.pos_skip_trivia();
        if self.tokens.get(idx).map(|(k, _)| *k) != Some(SyntaxKind::HASH) {
            return None;
        }
        idx += 1;
        while idx < self.tokens.len() && self.tokens[idx].0.is_trivia() {
            idx += 1;
        }
        match self.tokens.get(idx) {
            Some((SyntaxKind::IDENT, text)) => Some((*text).to_string()),
            _ => None,
        }
    }

    /// Parse a single method declaration inside a `with { ... }` block.
    /// Caller has already opened a SCHEMA_METHOD node and emitted any
    /// leading pragma directives. Shape:
    ///
    ///   IDENT GenericParams? '(' (Param (',' Param)*)? ')' '->' Type (':' Expr)?
    ///
    /// Each parameter takes the named form `name: Type` (opposite of
    /// `#main`'s `Type name`), reusing the existing CLOSURE_PARAM
    /// wrapper to keep the typed-AST layer simple. The body is omitted
    /// for `#native` methods.
    fn parse_schema_method_after_pragmas(&mut self) {
        // Method name.
        if self.at(SyntaxKind::IDENT) {
            self.bump();
        } else {
            self.error_at_current("expected method name");
            return;
        }
        // Optional method-level generics `<U, V>`.
        if self.at(SyntaxKind::LT) {
            self.bump();
            while !self.at(SyntaxKind::GT) && !self.at_end() {
                if self.at(SyntaxKind::IDENT) {
                    self.bump();
                } else {
                    self.error_at_current("expected method generic parameter");
                    break;
                }
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
            }
            self.expect(SyntaxKind::GT);
        }
        // Parameter list `(name: Type, ...)`.
        if !self.expect(SyntaxKind::L_PAREN) {
            return;
        }
        while !self.at(SyntaxKind::R_PAREN) && !self.at_end() {
            self.parse_schema_method_param();
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
        }
        self.expect(SyntaxKind::R_PAREN);
        // `-> ReturnType` — required by the analyzer-level grammar
        // (every with-block method declares its return), but the CST
        // accepts the missing-arrow shape so older test fixtures
        // that elided the return type still round-trip cleanly.
        if self.eat(SyntaxKind::THIN_ARROW) {
            self.parse_type();
        }
        // Optional `: body`. Methods marked `#native` omit it; for
        // others the analyzer enforces presence.
        if self.eat(SyntaxKind::COLON) {
            self.parse_expr();
        }
    }

    /// One schema-method parameter: `name: Type`. Lossless — emitted
    /// inside a CLOSURE_PARAM node so the typed-AST layer can reuse
    /// the existing wrapper.
    fn parse_schema_method_param(&mut self) {
        self.open(SyntaxKind::CLOSURE_PARAM);
        if self.at(SyntaxKind::IDENT) {
            self.bump();
        } else {
            self.error_at_current("expected parameter name");
            self.close();
            return;
        }
        if self.eat(SyntaxKind::COLON) {
            self.parse_type();
        } else {
            self.error("expected `:` in schema method parameter");
        }
        self.close();
    }

    /// `#import <spec> from "path"`. `<spec>` is one of
    /// `*`, `{ a, b as c }`, or a single identifier.
    fn parse_directive_import(&mut self) {
        if self.at(SyntaxKind::STAR) {
            self.bump();
        } else if self.at(SyntaxKind::L_BRACE) {
            // Destructure list `{ a, b as c }` — each entry is an
            // IDENT optionally followed by `as IDENT`. This is NOT a
            // dict, so we don't reuse `parse_dict`. The legacy
            // `parse_import_spec` accepts this shape; the typed-AST
            // layer carries the entries on `DirectiveImportSpec`.
            self.parse_import_destructure();
        } else if self.at(SyntaxKind::IDENT) {
            self.bump();
        } else {
            self.error_at_current("expected import spec");
            return;
        }
        if self.at(SyntaxKind::IDENT) && self.current_text() == Some("from") {
            self.bump();
        } else {
            self.error("expected `from` in #import");
            return;
        }
        if self.at(SyntaxKind::STRING) {
            self.bump();
        } else {
            self.error_at_current("expected path string in #import");
        }
    }

    fn parse_import_destructure(&mut self) {
        debug_assert!(self.at(SyntaxKind::L_BRACE));
        self.bump(); // {
        loop {
            if self.at(SyntaxKind::R_BRACE) || self.at_end() {
                break;
            }
            if self.at(SyntaxKind::IDENT) {
                self.bump();
                // Optional `as IDENT` alias.
                if self.at(SyntaxKind::IDENT) && self.current_text() == Some("as") {
                    self.bump();
                    if self.at(SyntaxKind::IDENT) {
                        self.bump();
                    } else {
                        self.error_at_current("expected identifier after `as` in #import");
                    }
                }
            } else {
                self.error_recover(
                    "expected identifier in #import destructure",
                    &[SyntaxKind::COMMA, SyntaxKind::R_BRACE],
                );
            }
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
        }
        self.expect(SyntaxKind::R_BRACE);
    }

    /// `#main ( type ident, ... ) [-> Type]`. Captures the typed
    /// param list directly so the directive node carries the same
    /// structure the analyzer needs.
    fn parse_directive_main(&mut self) {
        if !self.eat(SyntaxKind::L_PAREN) {
            self.error("expected `(` after `#main`");
            return;
        }
        while !self.at(SyntaxKind::R_PAREN) && !self.at_end() {
            // Each param: `Type ident` (closure-param shape).
            self.parse_closure_param();
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
        }
        self.expect(SyntaxKind::R_PAREN);
        // Optional `-> ReturnType`.
        if self.eat(SyntaxKind::THIN_ARROW) {
            self.parse_type();
        }
    }

    /// True when the next non-trivia token signals "no directive body
    /// here, leave the ident for the surrounding grammar" — used by
    /// `#schema X: value` (inside a dict) where `X` is the dict key,
    /// not the schema-name body.
    fn peek_attribute_terminator(&self) -> bool {
        let mut idx = self.pos_skip_trivia();
        // Skip an IDENT (and an optional generic angle-list).
        if self.tokens.get(idx).map(|(k, _)| *k) != Some(SyntaxKind::IDENT) {
            return false;
        }
        idx += 1;
        while idx < self.tokens.len() && self.tokens[idx].0.is_trivia() {
            idx += 1;
        }
        matches!(
            self.tokens.get(idx).map(|(k, _)| *k),
            Some(SyntaxKind::COLON) | Some(SyntaxKind::COMMA) | Some(SyntaxKind::R_BRACE)
        )
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
                    // `L_PAREN` covers the parenthesised closure form
                    // `(p) => body` and parenthesised expressions
                    // `(a + b)`. Without this, value-shape directives
                    // like `#default (self) => ...` and
                    // `#expect (n) => n > 0` would be parsed as
                    // body-less, leaving the closure for the
                    // surrounding dict to choke on.
                    | SyntaxKind::L_PAREN
                    | SyntaxKind::AMP
                    | SyntaxKind::MINUS
                    | SyntaxKind::BANG
                    | SyntaxKind::STAR
                    // F-strings start a fresh atom too.
                    | SyntaxKind::F_STRING_OPEN
            )
        })
    }

    // ----- expression entry -------------------------------------------

    /// Parse a full expression. Operator precedence is climbed with a
    /// Pratt-style loop. Lowest precedence first; primary handles
    /// atoms and prefix unaries. `match { ... }` and `where { ... }`
    /// trail the binary chain as the outermost postfix forms — they
    /// take precedence above ternary etc., matching the winnow
    /// grammar in `expr.rs`.
    fn parse_expr(&mut self) {
        let ck = self.checkpoint();
        self.parse_expr_bp(0);
        // Ternary: `cond ? then : else`. Bound at expression-tail
        // precedence — lower than every binary operator (so the binary
        // chain absorbs into `cond`) but higher than the trailing
        // `match` / `where` postfix forms (which wrap whatever ternary
        // produces). The legacy `parse_ternary` (`expr.rs`) sits at the
        // same level — see the precedence chain notes there.
        //
        // Disambiguation: `?` may also be a path-access prefix
        // (`a?.b`, `a?[0]`) or a type-optional marker (`Foo?` inside a
        // typed context). Path access is consumed earlier — the CST's
        // current postfix loop doesn't fold `?.` / `?[`, but the legacy
        // pre-P4 path always took those bytes itself, so no fixture
        // reaches this branch with them in postfix position. Type
        // optionals only appear inside committed `parse_type` calls
        // (match arms, closure params, directive bodies), never at the
        // outermost expression level — so seeing `?` here is
        // unambiguously a ternary head.
        if self.at(SyntaxKind::QUESTION) {
            // Guard: don't claim a ternary on `?.` / `?[`. Those forms
            // belong to path access and are handled (or rejected) by the
            // atom layer; consuming `?` here would steal the prefix.
            let next = self.nth(1);
            if !matches!(next, Some(SyntaxKind::DOT) | Some(SyntaxKind::L_BRACK)) {
                self.open_at(ck, SyntaxKind::TERNARY_EXPR);
                self.bump(); // ?
                self.parse_expr();
                if !self.expect(SyntaxKind::COLON) {
                    self.close();
                    return;
                }
                self.parse_expr();
                self.close();
            }
        }
        loop {
            if self.at(SyntaxKind::IDENT) && self.current_text() == Some("match") {
                // Only commit to MATCH_EXPR when `match` is followed
                // by `{` — otherwise it's a bareword called `match`
                // somewhere unrelated.
                if self.nth(1) == Some(SyntaxKind::L_BRACE) {
                    self.open_at(ck, SyntaxKind::MATCH_EXPR);
                    self.bump(); // `match`
                    self.bump(); // {
                    while !self.at(SyntaxKind::R_BRACE) && !self.at_end() {
                        self.parse_match_arm();
                        if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_BRACE) {
                            self.error_recover(
                                "expected `,` or `}` in match",
                                &[SyntaxKind::COMMA, SyntaxKind::R_BRACE],
                            );
                            self.eat(SyntaxKind::COMMA);
                        }
                    }
                    self.expect(SyntaxKind::R_BRACE);
                    self.close();
                    continue;
                }
            }
            if self.at(SyntaxKind::IDENT)
                && self.current_text() == Some("where")
                && self.nth(1) == Some(SyntaxKind::L_BRACE)
            {
                self.open_at(ck, SyntaxKind::WHERE_EXPR);
                self.bump(); // `where`
                self.parse_dict();
                self.close();
                continue;
            }
            break;
        }
    }

    /// One match arm: `pattern: body`. Pattern is either a TYPE_NODE
    /// (the common case) or `*` (the wildcard fallback). Body is a
    /// regular expression.
    fn parse_match_arm(&mut self) {
        self.open(SyntaxKind::MATCH_ARM);
        if self.at(SyntaxKind::STAR) {
            self.open(SyntaxKind::WILDCARD);
            self.bump();
            self.close();
        } else if self.at(SyntaxKind::IDENT) {
            self.parse_type();
        } else {
            self.error_at_current("expected match-arm pattern");
        }
        if self.eat(SyntaxKind::COLON) {
            self.parse_expr();
        } else {
            self.error("expected `:` in match arm");
        }
        self.close();
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

    /// Atom with postfix suffixes (`.field`, `[i]`, `(args)`,
    /// plus optional-chain `?.field` / `?[i]`).
    fn parse_postfix(&mut self) {
        let ck = self.checkpoint();
        self.parse_atom();
        loop {
            if self.at(SyntaxKind::L_PAREN) {
                self.open_at(ck, SyntaxKind::CALL_EXPR);
                self.parse_call_args();
                self.close();
            } else if self.at(SyntaxKind::DOT)
                || self.at(SyntaxKind::L_BRACK)
                || (self.at(SyntaxKind::QUESTION)
                    && matches!(
                        self.nth(1),
                        Some(SyntaxKind::DOT) | Some(SyntaxKind::L_BRACK)
                    ))
            {
                // Path access — fold into VARIABLE_EXPR so dotted
                // paths like `a.b.c` end up as a single node. v1.8
                // positional access `xs.0` (number after `.`) is the
                // tuple/list index form — accepted alongside `.field`.
                // Optional chaining (`a?.b`, `a?[0]`) consumes the `?`
                // as a prefix on the next segment; the typed-AST
                // marks the segment as optional.
                self.open_at(ck, SyntaxKind::VARIABLE_EXPR);
                loop {
                    let is_optional_prefix = self.at(SyntaxKind::QUESTION)
                        && matches!(
                            self.nth(1),
                            Some(SyntaxKind::DOT) | Some(SyntaxKind::L_BRACK)
                        );
                    if is_optional_prefix {
                        self.bump(); // ?
                    } else if !self.at(SyntaxKind::DOT) && !self.at(SyntaxKind::L_BRACK) {
                        break;
                    }
                    if self.at(SyntaxKind::DOT) {
                        self.bump();
                        if self.at(SyntaxKind::IDENT) || self.at(SyntaxKind::NUMBER) {
                            self.bump();
                        } else {
                            self.error_at_current("expected identifier or index after `.`");
                        }
                    } else if self.at(SyntaxKind::L_BRACK) {
                        // `[ index ]`
                        self.bump();
                        self.parse_expr();
                        self.expect(SyntaxKind::R_BRACK);
                    } else {
                        break;
                    }
                }
                self.close();
            } else {
                break;
            }
        }
    }

    fn parse_atom(&mut self) {
        // Leading attributes (`#brand T {...}` / `@decorator(x) expr`)
        // stack above the atom they decorate. The CST keeps them as
        // siblings of the atom inside whatever node the caller opened
        // (typically a DICT_FIELD value, a LIST element, or a function
        // argument). The legacy parser handled this case the same way
        // — the attribute decorates whatever expression follows.
        while self.at(SyntaxKind::HASH) || self.at(SyntaxKind::AT) {
            // Guard: when `#` heads a directive whose body is bare
            // (e.g. `#strict` standing alone at file scope), there's
            // no following expression — `parse_attribute` consumes
            // nothing extra, and the loop would spin. Break out the
            // moment we see no progress.
            let before = self.pos;
            self.parse_attribute();
            if self.pos == before {
                break;
            }
        }
        match self.current() {
            Some(SyntaxKind::NUMBER) => {
                self.open(SyntaxKind::LITERAL);
                self.bump();
                self.close();
            }
            Some(SyntaxKind::STRING) => {
                let text = self.tokens[self.pos_skip_trivia()].1;
                if text.starts_with('f') {
                    self.parse_f_string();
                } else {
                    self.open(SyntaxKind::LITERAL);
                    self.bump();
                    self.close();
                }
            }
            Some(SyntaxKind::IDENT) => {
                // `null` / `true` / `false` are keyword-shaped
                // literals but lex as IDENT — promote here.
                let text = self.tokens[self.pos_skip_trivia()].1;
                if matches!(text, "null" | "true" | "false") {
                    self.open(SyntaxKind::LITERAL);
                    self.bump();
                    self.close();
                } else if self.looks_like_variant_ctor() {
                    // `Enum.Variant { ... }` — at least two dotted
                    // segments followed by a brace body. Legacy
                    // `parse_variant_ctor` requires `path.len() >= 2`
                    // before committing; we match that here so plain
                    // `foo.bar` member access still falls through to
                    // the postfix loop as VARIABLE_EXPR.
                    self.parse_variant_ctor();
                } else if self.looks_like_type_atom() {
                    // Bareword type expressions (`Dict<String, Int>`,
                    // `List<Int>`, `Foo?`). Legacy `parse_type_expr`
                    // lowers these into `Expr::Type`; we follow suit so
                    // forms like `#brand Dict<String, Int> { ... }`
                    // and `#schema Status Enum<"on", "off">` parse
                    // cleanly without the Pratt grammar misreading
                    // `<` as a comparison.
                    self.parse_type();
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
                // Two shapes share the leading `(`:
                //   1. `(p1, p2) [-> RetType] => body` — a closure.
                //   2. `(expr)`                       — a parenthesised
                //      group (or, theoretically, a tuple, but the v1
                //      grammar treats tuples only as TYPE expressions).
                // We use lookahead to commit to the closure shape only
                // when we can see the trailing `=>` (after the optional
                // return type) — anything else stays as a group so the
                // round-trip never regresses on edge cases.
                if self.try_parse_paren_closure() {
                    return;
                }
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
                // v1.3 typed spread: `...<Type> expr`. The type hint
                // sits between the ellipsis and the source expression
                // and disambiguates strict-mode derivation. The inner
                // expression follows the type with no separator.
                if self.at(SyntaxKind::LT) {
                    self.bump();
                    self.parse_type();
                    self.expect(SyntaxKind::GT);
                }
                self.parse_unary();
                self.close();
            }
            _ => self.error_at_current("expected expression"),
        }
    }

    /// Look ahead past the current IDENT for an `IDENT (DOT IDENT)+ {`
    /// sequence — the variant-constructor shape `Enum.Variant { ... }`
    /// the legacy `parse_variant_ctor` (`expr.rs`) detects. Returns
    /// true only when at least two dotted segments precede the `{`,
    /// matching the legacy `path.len() < 2` guard. Anything else
    /// (single-segment IDENT, dotted-path member access without a
    /// trailing brace) falls through to the regular VARIABLE_EXPR path.
    fn looks_like_variant_ctor(&self) -> bool {
        if !self.at(SyntaxKind::IDENT) {
            return false;
        }
        let mut idx = self.pos_skip_trivia() + 1;
        let advance_trivia = |i: &mut usize, toks: &[(SyntaxKind, &str)]| {
            while *i < toks.len() && toks[*i].0.is_trivia() {
                *i += 1;
            }
        };
        advance_trivia(&mut idx, &self.tokens);
        let mut segs: usize = 1;
        while self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::DOT) {
            idx += 1;
            advance_trivia(&mut idx, &self.tokens);
            if self.tokens.get(idx).map(|(k, _)| *k) != Some(SyntaxKind::IDENT) {
                return false;
            }
            idx += 1;
            segs += 1;
            advance_trivia(&mut idx, &self.tokens);
        }
        if segs < 2 {
            return false;
        }
        self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::L_BRACE)
    }

    /// Decide whether the current IDENT atom heads a *type* expression
    /// (`Dict<String, Int>`, `List<Int>`, `Foo?`). Legacy
    /// `parse_type_expr` (`expr.rs`) lowers such atoms into
    /// `Expr::Type`; downstream forms like `#brand Dict<K, V> { ... }`
    /// rely on this so the value body isn't misread as `Dict < K`
    /// (binary comparison).
    ///
    /// Conservative: only fires when the type-ness signal is
    /// unambiguous — the IDENT is a known type head, OR is
    /// immediately followed by `<...>` generics (no whitespace
    /// before `<`), with the angle balance closing cleanly. A
    /// trailing `?` (optional marker) also qualifies.
    fn looks_like_type_atom(&self) -> bool {
        if !self.at(SyntaxKind::IDENT) {
            return false;
        }
        let head_text = self.current_text().unwrap_or("");
        let head_idx = self.pos_skip_trivia();
        let mut idx = head_idx + 1;
        let mut had_ws = false;
        while idx < self.tokens.len() && self.tokens[idx].0.is_trivia() {
            had_ws = true;
            idx += 1;
        }
        let known_head = matches!(
            head_text,
            "Int" | "String" | "Bool" | "Float" | "Any" | "Null" | "List" | "Dict" | "Enum"
        );
        // `IDENT < ...>` — type with generics. Requires `<`
        // immediately adjacent (no whitespace).
        if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::LT) && !had_ws {
            // Scan for the matching `>` while tracking parens.
            let mut depth: i32 = 1;
            let mut paren_depth: i32 = 0;
            let mut j = idx + 1;
            while j < self.tokens.len() && depth > 0 {
                match self.tokens[j].0 {
                    SyntaxKind::LT => depth += 1,
                    SyntaxKind::GT => depth -= 1,
                    SyntaxKind::L_PAREN => paren_depth += 1,
                    SyntaxKind::R_PAREN if paren_depth > 0 => paren_depth -= 1,
                    SyntaxKind::L_BRACE
                    | SyntaxKind::R_BRACE
                    | SyntaxKind::R_PAREN
                    | SyntaxKind::FAT_ARROW
                        if depth == 1 && paren_depth == 0 =>
                    {
                        return false
                    }
                    _ => {}
                }
                j += 1;
            }
            return depth == 0;
        }
        // Bare type head with no generics — only fires when the IDENT
        // is recognised as a primitive type name. Guarded by what
        // follows so plain VARIABLE_EXPR usage doesn't accidentally
        // become a TYPE_NODE: must be followed by `{` (type-tagged
        // dict body, `#brand T { ... }`) or `?` (optional marker).
        if known_head {
            let next = self.tokens.get(idx).map(|(k, _)| *k);
            if matches!(next, Some(SyntaxKind::QUESTION) | Some(SyntaxKind::L_BRACE)) {
                return true;
            }
        }
        // `IDENT ? {` — user-defined schema with optional marker and
        // an immediately-following brace body (`Weather? { ... }`).
        // The optional `?` plus brace makes this unambiguously a
        // type-tagged value, never a ternary expression head.
        if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::QUESTION) {
            let mut j = idx + 1;
            while j < self.tokens.len() && self.tokens[j].0.is_trivia() {
                j += 1;
            }
            if self.tokens.get(j).map(|(k, _)| *k) == Some(SyntaxKind::L_BRACE) {
                return true;
            }
        }
        false
    }

    /// `Enum (.Variant)+ { body }` — emit a VARIANT_CTOR node wrapping
    /// the dotted path (as plain IDENT + DOT tokens) and the brace
    /// body (a regular DICT). Caller has already determined via
    /// [`Self::looks_like_variant_ctor`] that we're at the head IDENT
    /// of such a construct.
    fn parse_variant_ctor(&mut self) {
        self.open(SyntaxKind::VARIANT_CTOR);
        // Head IDENT.
        self.bump();
        // Drain `.IDENT*` — guaranteed at least one by the peek.
        while self.at(SyntaxKind::DOT) {
            self.bump();
            if self.at(SyntaxKind::IDENT) {
                self.bump();
            } else {
                self.error_at_current("expected identifier after `.` in variant constructor");
                break;
            }
        }
        // Body is a regular dict literal.
        if self.at(SyntaxKind::L_BRACE) {
            self.parse_dict();
        } else {
            self.error("expected `{` in variant constructor");
        }
        self.close();
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

    /// Decompose a leading `f"..."` / `f#"..."#` STRING token into a
    /// proper [`F_STRING`] subtree. The original token is consumed
    /// as a SINGLE leaf at the lex level, but for the CST we walk
    /// its bytes and emit:
    ///
    /// * `F_STRING_OPEN` — `f"` / `f#"` / `f##"` …
    /// * `F_STRING_LITERAL` — verbatim text between zones.
    /// * `F_STRING_INTERPOLATION` (a sub-node) — wraps a
    ///   `F_STRING_INTERP_START`, a recursively-parsed expression
    ///   (using the same flat lex on the interpolation bytes), and a
    ///   `F_STRING_INTERP_END`.
    /// * `F_STRING_CLOSE` — matching `"` / `"#` / `"##` …
    ///
    /// Reuses [`lex::lex`] for the interpolation bytes so any future
    /// lexer change is picked up automatically. The whole emission is
    /// driven directly by the original byte span — so the round-trip
    /// invariant holds without help from the caller.
    fn parse_f_string(&mut self) {
        // Flush trivia FIRST so the F_STRING node nests under whatever
        // production opened most recently. We then refuse to advance
        // `self.pos` until we've emitted every sub-piece, so the
        // overall byte count matches the original STRING token.
        self.flush_trivia();
        let tok_idx = self.pos;
        let (_kind, full_text): (SyntaxKind, &'a str) = self.tokens[tok_idx];
        let start_byte = self.cursor_byte;
        // Parse the opening sequence: `f` + zero-or-more `#` + `"`.
        let bytes = full_text.as_bytes();
        // The lexer already guarantees this token starts with `f`,
        // and that `next_is_hash_quote(bytes, 1)` was true, but be
        // defensive — bail to plain LITERAL if anything else.
        if bytes.first() != Some(&b'f') {
            // Should be unreachable given the caller's guard.
            self.open(SyntaxKind::LITERAL);
            self.bump();
            self.close();
            return;
        }
        let mut idx: usize = 1;
        while bytes.get(idx) == Some(&b'#') {
            idx += 1;
        }
        if bytes.get(idx) != Some(&b'"') {
            // Malformed open — emit the whole thing as a single
            // LITERAL so byte-round-trip is preserved.
            self.open(SyntaxKind::LITERAL);
            self.bump();
            self.close();
            return;
        }
        let hash_count = idx - 1;
        let open_end = idx + 1;
        let mut closing = String::from("\"");
        for _ in 0..hash_count {
            closing.push('#');
        }

        // Locate the close. The body starts at `open_end`; we have to
        // track interpolation depth so a literal `}` inside an
        // interpolation can't be mistaken for the close.
        let body_start = open_end;
        let close_pos = self.find_fstring_close(bytes, body_start, &closing, hash_count);
        let close_pos = match close_pos {
            Some(p) => p,
            None => {
                // Unterminated — fall back to LITERAL.
                self.open(SyntaxKind::LITERAL);
                self.bump();
                self.close();
                return;
            }
        };

        // Open the composite node.
        self.open(SyntaxKind::F_STRING);
        // Emit OPEN.
        self.emit_raw_token(SyntaxKind::F_STRING_OPEN, &full_text[..open_end]);
        // Walk body, splitting LITERAL chunks vs interpolation zones.
        let mut i = body_start;
        let mut literal_start = i;
        let raw_string = hash_count > 0;
        while i < close_pos {
            if Self::starts_with_at(bytes, i, b"${") {
                if i > literal_start {
                    self.emit_raw_token(SyntaxKind::F_STRING_LITERAL, &full_text[literal_start..i]);
                }
                // Find matching `}`.
                let interp_start = i;
                let interp_body_start = i + 2;
                let mut depth: usize = 1;
                let mut j = interp_body_start;
                while j < close_pos && depth > 0 {
                    match bytes[j] {
                        b'{' => {
                            depth += 1;
                            j += 1;
                        }
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            j += 1;
                        }
                        b'"' => {
                            // Skip nested "..." (the lexer always
                            // pairs them up safely on round-trip).
                            j = crate::lex::scan_normal_string_for_cst(bytes, j);
                        }
                        b => {
                            // Skip a full codepoint to make progress
                            // on invalid UTF-8 boundaries.
                            j += utf8_codepoint_len(b);
                        }
                    }
                }
                if depth != 0 {
                    // Unterminated interpolation — emit the rest as
                    // one literal so bytes survive, then stop.
                    self.emit_raw_token(SyntaxKind::F_STRING_LITERAL, &full_text[i..close_pos]);
                    literal_start = close_pos;
                    break;
                }
                let interp_body_end = j;
                let interp_close = j + 1;
                // Emit the interpolation sub-node.
                self.open(SyntaxKind::F_STRING_INTERPOLATION);
                self.emit_raw_token(
                    SyntaxKind::F_STRING_INTERP_START,
                    &full_text[interp_start..interp_body_start],
                );
                // Sub-parse the inner expression. The inner text is a
                // self-contained slice; we hand it to a fresh `lex` +
                // mini-parser. This is recursive (an interpolation can
                // contain another f-string), but the byte-accounting
                // works because we splice sub-tokens directly into the
                // builder.
                self.parse_fstring_interp_inner(&full_text[interp_body_start..interp_body_end]);
                self.emit_raw_token(
                    SyntaxKind::F_STRING_INTERP_END,
                    &full_text[interp_body_end..interp_close],
                );
                self.close();
                literal_start = interp_close;
                i = interp_close;
                continue;
            }
            // Escape handling — only relevant in non-raw f-strings.
            if !raw_string && bytes[i] == b'\\' && i + 1 < close_pos {
                i += 1 + utf8_codepoint_len(bytes[i + 1]);
                continue;
            }
            i += utf8_codepoint_len(bytes[i]);
        }
        if literal_start < close_pos {
            self.emit_raw_token(
                SyntaxKind::F_STRING_LITERAL,
                &full_text[literal_start..close_pos],
            );
        }
        // Emit CLOSE.
        self.emit_raw_token(SyntaxKind::F_STRING_CLOSE, &full_text[close_pos..]);
        self.close();
        // Advance the parser past the original STRING token now that
        // we've emitted every sub-piece directly.
        self.cursor_byte = start_byte + full_text.len();
        self.pos = tok_idx + 1;
    }

    /// Emit a single leaf token directly to the builder (bypassing
    /// the lex-token cursor). Used by f-string decomposition; never
    /// advances `pos` / `cursor_byte`.
    fn emit_raw_token(&mut self, kind: SyntaxKind, text: &str) {
        self.builder
            .token(RelonLanguage::kind_to_raw_static(kind), text);
    }

    /// Sub-parser for the inside of `${ ... }` in an f-string. We
    /// temporarily swap `self.tokens` with the inner-text lex (the
    /// `&str` slices inside still borrow from the original source,
    /// so the swapped `Vec` is fully compatible lifetime-wise),
    /// run the same Pratt expression grammar, then restore.
    fn parse_fstring_interp_inner(&mut self, text: &'a str) {
        let inner_tokens: Vec<(SyntaxKind, &'a str)> = crate::lex::lex(text);
        // Stash outer state and install the inner stream.
        let outer_tokens = std::mem::replace(&mut self.tokens, inner_tokens);
        let outer_pos = std::mem::replace(&mut self.pos, 0);
        let outer_cursor = self.cursor_byte;
        self.cursor_byte = 0;
        if !self.at_end() {
            self.parse_expr();
        }
        // Absorb any remaining bytes so the F_STRING_INTERPOLATION
        // body has full byte coverage. Trailing whitespace becomes
        // trivia naturally; anything else lands in an ERROR node.
        if !self.at_end() {
            self.error_recover("trailing input in interpolation", &[]);
        }
        self.flush_trivia();
        // Restore outer state.
        self.tokens = outer_tokens;
        self.pos = outer_pos;
        self.cursor_byte = outer_cursor + text.len();
    }

    fn find_fstring_close(
        &self,
        bytes: &[u8],
        body_start: usize,
        closing: &str,
        hashes: usize,
    ) -> Option<usize> {
        let raw = hashes > 0;
        let mut idx = body_start;
        while idx + closing.len() <= bytes.len() {
            // Skip past balanced `${...}` interpolations.
            if Self::starts_with_at(bytes, idx, b"${") {
                let mut depth: usize = 1;
                let mut j = idx + 2;
                while j < bytes.len() && depth > 0 {
                    match bytes[j] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        b'"' => {
                            j = crate::lex::scan_normal_string_for_cst(bytes, j);
                            continue;
                        }
                        _ => {}
                    }
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                if depth != 0 {
                    return None;
                }
                idx = j;
                continue;
            }
            if !raw && bytes[idx] == b'\\' {
                if idx + 1 >= bytes.len() {
                    return None;
                }
                idx += 1 + utf8_codepoint_len(bytes[idx + 1]);
                continue;
            }
            if Self::starts_with_at(bytes, idx, closing.as_bytes()) {
                return Some(idx);
            }
            idx += utf8_codepoint_len(bytes[idx]);
        }
        None
    }

    fn starts_with_at(bytes: &[u8], idx: usize, needle: &[u8]) -> bool {
        bytes
            .get(idx..idx + needle.len())
            .is_some_and(|s| s == needle)
    }

    /// Scan forward (without committing) starting from `start_idx`,
    /// past a balanced `(...)`, returning the index of the first
    /// non-trivia token AFTER the closing `)`. `start_idx` must point
    /// at the opening `L_PAREN` token. Returns `None` if the parens
    /// are unbalanced (we ran past EOI before matching).
    fn scan_after_matching_paren(&self, start_idx: usize) -> Option<usize> {
        debug_assert!(self.tokens.get(start_idx).map(|(k, _)| *k) == Some(SyntaxKind::L_PAREN));
        let mut depth: i32 = 0;
        let mut idx = start_idx;
        while idx < self.tokens.len() {
            let kind = self.tokens[idx].0;
            match kind {
                SyntaxKind::L_PAREN => depth += 1,
                SyntaxKind::R_PAREN => {
                    depth -= 1;
                    if depth == 0 {
                        let mut next = idx + 1;
                        while next < self.tokens.len() && self.tokens[next].0.is_trivia() {
                            next += 1;
                        }
                        return Some(next);
                    }
                }
                _ => {}
            }
            idx += 1;
        }
        None
    }

    /// Without consuming anything, decide whether the `(...)` at the
    /// current position is followed (modulo an optional `-> Type`) by
    /// a `=>` arrow — i.e. the parens are a closure parameter list,
    /// not a parenthesised expression. We're already at the
    /// `L_PAREN`.
    fn looks_like_closure_after_paren(&self) -> bool {
        let lparen_idx = self.pos_skip_trivia();
        let Some(after_paren) = self.scan_after_matching_paren(lparen_idx) else {
            return false;
        };
        // `=> ...`?
        if matches!(
            self.tokens.get(after_paren).map(|(k, _)| *k),
            Some(SyntaxKind::FAT_ARROW)
        ) {
            return true;
        }
        // `-> RetType => ...`? Skip past the return-type tokens. We
        // can't fully parse a type without committing, so scan ahead
        // conservatively until we hit `=>` (closure) or anything that
        // disqualifies (newline-like break is fine — trivia is skipped
        // by definition, but we treat `,`/`}`/`]`/`)`/`:` as a
        // disqualifier so we never confuse `-> Type:` patterns).
        if matches!(
            self.tokens.get(after_paren).map(|(k, _)| *k),
            Some(SyntaxKind::THIN_ARROW)
        ) {
            let mut idx = after_paren + 1;
            let mut bracket_depth: i32 = 0;
            while idx < self.tokens.len() {
                let kind = self.tokens[idx].0;
                if kind.is_trivia() {
                    idx += 1;
                    continue;
                }
                match kind {
                    SyntaxKind::FAT_ARROW if bracket_depth == 0 => return true,
                    SyntaxKind::COMMA
                    | SyntaxKind::R_BRACE
                    | SyntaxKind::R_BRACK
                    | SyntaxKind::R_PAREN
                    | SyntaxKind::COLON
                        if bracket_depth == 0 =>
                    {
                        return false
                    }
                    SyntaxKind::L_BRACE
                    | SyntaxKind::L_BRACK
                    | SyntaxKind::L_PAREN
                    | SyntaxKind::LT => {
                        bracket_depth += 1;
                    }
                    SyntaxKind::R_BRACE | SyntaxKind::R_BRACK | SyntaxKind::GT => {
                        if bracket_depth > 0 {
                            bracket_depth -= 1;
                        }
                    }
                    _ => {}
                }
                idx += 1;
            }
        }
        false
    }

    /// When `current()` is `L_PAREN` and `looks_like_closure_after_paren`
    /// is true, consume the entire `(params) [-> RetType] => body`
    /// construct and emit a CLOSURE node. Returns true on success.
    /// Leaves the parser untouched and returns false otherwise.
    fn try_parse_paren_closure(&mut self) -> bool {
        if !self.at(SyntaxKind::L_PAREN) {
            return false;
        }
        if !self.looks_like_closure_after_paren() {
            return false;
        }
        self.open(SyntaxKind::CLOSURE);
        self.bump(); // (
                     // Comma-separated CLOSURE_PARAMs.
        while !self.at(SyntaxKind::R_PAREN) && !self.at_end() {
            self.parse_closure_param();
            if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_PAREN) {
                self.error_recover(
                    "expected `,` or `)` in closure parameter list",
                    &[SyntaxKind::COMMA, SyntaxKind::R_PAREN],
                );
                self.eat(SyntaxKind::COMMA);
            }
        }
        self.expect(SyntaxKind::R_PAREN);
        // Optional `-> RetType`.
        if self.eat(SyntaxKind::THIN_ARROW) {
            self.parse_type();
        }
        if self.expect(SyntaxKind::FAT_ARROW) {
            self.parse_expr();
        }
        self.close();
        true
    }

    /// One closure parameter — either `name` or `Type name`. P2
    /// records the type, when present, as a TYPE_NODE child preceding
    /// the IDENT.
    fn parse_closure_param(&mut self) {
        self.open(SyntaxKind::CLOSURE_PARAM);
        // Heuristic: if the next two non-trivia tokens are IDENT IDENT
        // (or a more elaborate type followed by an ident), treat the
        // leading run as a TypeNode. We delegate to `parse_type` which
        // commits conservatively (it stops at the first non-type-y
        // token, so a bare `IDENT` doesn't get swallowed as a type).
        // The simplest signal of "this is a typed param" is that
        // there are at least two adjacent IDENTs, possibly with `<...>`
        // / `?` in the type slot.
        if self.peek_is_typed_param() {
            self.parse_type();
        }
        if self.at(SyntaxKind::IDENT) {
            self.bump();
        } else {
            self.error_at_current("expected closure parameter name");
        }
        self.close();
    }

    /// Cheap lookahead: does the upcoming token stream look like
    /// `Type ident` (a typed closure parameter) or just `ident`
    /// (untyped)? We say "typed" if the current token is IDENT and
    /// the next non-trivia token after a `Type`-shaped run is another
    /// IDENT — meaning the first one is the type and the second is
    /// the param name. We allow `<...>` and `?` between them.
    ///
    /// Crucial heuristic: when a `<` appears, it must be immediately
    /// adjacent (no whitespace) to the preceding IDENT for it to
    /// count as opening a generic argument list. Without this
    /// guard, `a < b: c` (a closure param of type `a` named `< b`
    /// — but `<` isn't a valid name leader, so it bails)
    /// would still be misinterpreted in pathological cases. Rust /
    /// TypeScript both use the same lex-time adjacency check.
    fn peek_is_typed_param(&self) -> bool {
        if !self.at(SyntaxKind::IDENT) {
            return false;
        }
        // Walk past IDENT, optional `.IDENT*`, optional `<...>`,
        // optional `?`, then check for IDENT.
        let head_idx = self.pos_skip_trivia();
        let mut idx = head_idx + 1;
        let advance_trivia = |i: &mut usize| {
            while *i < self.tokens.len() && self.tokens[*i].0.is_trivia() {
                *i += 1;
            }
        };
        // For the adjacency check we want to know whether ANY trivia
        // intervenes between the IDENT and the next non-trivia token.
        let mut had_trivia_after_head = false;
        if idx < self.tokens.len() && self.tokens[idx].0.is_trivia() {
            had_trivia_after_head = true;
            advance_trivia(&mut idx);
        }
        // `.IDENT*`
        while idx < self.tokens.len() && self.tokens[idx].0 == SyntaxKind::DOT {
            idx += 1;
            advance_trivia(&mut idx);
            if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::IDENT) {
                idx += 1;
                advance_trivia(&mut idx);
            } else {
                return false;
            }
            had_trivia_after_head = false;
        }
        // `<...>` — balanced angle scan. Refuse when whitespace
        // separates the IDENT and the `<` — that's the disambiguation
        // hook between `Foo<Bar>` (type) and `a < b` (comparison).
        if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::LT) {
            if had_trivia_after_head {
                return false;
            }
            let mut depth: i32 = 1;
            // Track nested `(...)` so tuple-type arguments like
            // `List<(Int, String)>` don't trip the comma rejection.
            let mut paren_depth: i32 = 0;
            idx += 1;
            while idx < self.tokens.len() && depth > 0 {
                match self.tokens[idx].0 {
                    SyntaxKind::LT => depth += 1,
                    SyntaxKind::GT => depth -= 1,
                    SyntaxKind::L_PAREN => paren_depth += 1,
                    SyntaxKind::R_PAREN if paren_depth > 0 => paren_depth -= 1,
                    // Anything that strongly disqualifies a type
                    // expression — bail. Commas at depth==1 are
                    // fine (`Dict<String, Int>`) — only structural
                    // tokens that can never appear inside a type
                    // disqualify the scan.
                    SyntaxKind::L_BRACE
                    | SyntaxKind::R_BRACE
                    | SyntaxKind::R_PAREN
                    | SyntaxKind::FAT_ARROW
                        if depth == 1 && paren_depth == 0 =>
                    {
                        return false
                    }
                    _ => {}
                }
                idx += 1;
            }
            if depth != 0 {
                return false;
            }
            advance_trivia(&mut idx);
        }
        // Optional `?`.
        if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::QUESTION) {
            idx += 1;
            advance_trivia(&mut idx);
        }
        self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::IDENT)
    }

    /// Parse a type-expression-shaped run of tokens into a TYPE_NODE.
    /// The grammar:
    ///
    ///   TypeNode    := TupleType | (PathSeg ('.' PathSeg)* GenericArgs? '?'?)
    ///   TupleType   := '(' ')' | '(' TypeNode ',' ')' | '(' TypeNode (',' TypeNode)+ ','? ')'
    ///   PathSeg     := IDENT | STRING
    ///   GenericArgs := '<' (TypeNode (',' TypeNode)*)? ','? '>'
    ///
    /// Matches the winnow `parse_type_node` (in `expr.rs`) for every
    /// shape the corpus uses today, including string-keyed segments
    /// (`"namespaced".Foo`), nested generics (`Map<String, Int>`),
    /// the optional `?` marker, and v1.7 tuple types in both
    /// type-hint position (`(Int, String) pair: ...`) and as generic
    /// arguments (`List<(Int, String)>`).
    fn parse_type(&mut self) {
        // Tuple type — committed only when the caller picked
        // `parse_type` (typed-key / generic-arg / closure-param /
        // return-type position). The expression grammar uses its own
        // `(...)` handler so a parens group never reaches this branch.
        if self.at(SyntaxKind::L_PAREN) {
            self.parse_tuple_type();
            return;
        }
        self.open(SyntaxKind::TYPE_NODE);
        // First segment: IDENT or STRING (allowed in the v1 grammar
        // for dotted-string paths like `"foo".Bar`).
        if self.at(SyntaxKind::IDENT) || self.at(SyntaxKind::STRING) {
            self.bump();
        } else {
            self.error_at_current("expected type name");
            self.close();
            return;
        }
        // Dotted continuation.
        while self.at(SyntaxKind::DOT) {
            self.bump();
            if self.at(SyntaxKind::IDENT) || self.at(SyntaxKind::STRING) {
                self.bump();
            } else {
                self.error_at_current("expected identifier after `.` in type");
            }
        }
        // Generic argument list. We're in a committed type context
        // here (the caller already decided "this is a type"), so any
        // `<` opens generics — no adjacency check needed.
        if self.at(SyntaxKind::LT) {
            self.bump();
            loop {
                if self.at(SyntaxKind::GT) || self.at_end() {
                    break;
                }
                self.parse_type();
                // `Enum<Variant { field: T, ... }, ...>` — a struct-
                // variant body inside a sum-type's generic-arg list.
                // The body is a dict of field-type pairs; we accept
                // any DICT here and let the analyzer enforce shape.
                if self.at(SyntaxKind::L_BRACE) {
                    self.parse_dict();
                }
                if !self.eat(SyntaxKind::COMMA) {
                    break;
                }
            }
            self.expect(SyntaxKind::GT);
        }
        // Optional `?` (i.e. `User?`).
        if self.at(SyntaxKind::QUESTION) {
            self.bump();
        }
        self.close();
    }

    /// `(T1, T2, ...)` tuple type. Three shapes:
    ///
    /// * `()`         — zero-tuple.
    /// * `(T,)`       — one-tuple (trailing comma is mandatory; without
    ///                  it the form is a parenthesised type, not used
    ///                  in the current grammar but still consumed as
    ///                  a single-element TUPLE_TYPE for forward-compat).
    /// * `(T1, T2)`   — 2+ tuple, optional trailing comma.
    ///
    /// Caller has already committed to type-position via `parse_type`,
    /// so we don't have to worry about confusing this with a closure
    /// param list — the closure detection happens at the expression
    /// layer (`try_parse_paren_closure`) and never reaches here.
    fn parse_tuple_type(&mut self) {
        self.open(SyntaxKind::TUPLE_TYPE);
        self.bump(); // (
        while !self.at(SyntaxKind::R_PAREN) && !self.at_end() {
            self.parse_type();
            if !self.eat(SyntaxKind::COMMA) {
                break;
            }
        }
        self.expect(SyntaxKind::R_PAREN);
        // Tuple types support the same trailing `?` as regular types
        // (`(Int, String)?` — nullable pair).
        if self.at(SyntaxKind::QUESTION) {
            self.bump();
        }
        self.close();
    }

    fn parse_reference(&mut self) {
        // `&base.tail.tail...` with optional-chain `?.` / `?[` access
        // forms (`&a.b?.c`, `&a?.[0]`). The legacy `reference_var`
        // grammar accepts both `.` / `[` and the `?`-prefixed variant
        // — the typed-AST tags the optional-ness on each `TokenKey`.
        self.open(SyntaxKind::REFERENCE_EXPR);
        self.bump(); // &
        if self.at(SyntaxKind::IDENT) {
            self.bump(); // base name
        } else {
            self.error_at_current("expected reference base after `&`");
        }
        loop {
            // `?.` and `?[` — eat the `?` prefix first, then fall
            // through to the regular dot / bracket handling.
            if self.at(SyntaxKind::QUESTION)
                && matches!(self.nth(1), Some(SyntaxKind::DOT) | Some(SyntaxKind::L_BRACK))
            {
                self.bump(); // ?
            } else if !self.at(SyntaxKind::DOT) && !self.at(SyntaxKind::L_BRACK) {
                break;
            }
            if self.at(SyntaxKind::DOT) {
                self.bump();
                if self.at(SyntaxKind::IDENT) || self.at(SyntaxKind::NUMBER) {
                    self.bump();
                } else {
                    self.error_at_current("expected identifier or index after `.`");
                }
            } else if self.at(SyntaxKind::L_BRACK) {
                self.bump(); // [
                self.parse_expr();
                self.expect(SyntaxKind::R_BRACK);
            } else {
                break;
            }
        }
        self.close();
    }

    fn parse_list(&mut self) {
        // We don't know up-front whether this `[` opens a list or a
        // comprehension — comprehensions look like `[ expr for id in
        // iterable (if cond)? ]`. Use a checkpoint so we can wrap the
        // first expression into either LIST or COMPREHENSION based on
        // what we find next.
        let outer_ck = self.checkpoint();
        self.bump(); // [
                     // Empty list — handle explicitly so we don't try to parse an
                     // expression after `[`.
        if self.at(SyntaxKind::R_BRACK) {
            self.open_at(outer_ck, SyntaxKind::LIST);
            self.bump();
            self.close();
            return;
        }
        // Parse the first element (or `for` head). If it's a spread,
        // it can't be a comprehension head — emit LIST directly.
        if self.at(SyntaxKind::ELLIPSIS) {
            self.open_at(outer_ck, SyntaxKind::LIST);
            self.parse_list_body_tail();
            return;
        }
        self.parse_expr();
        // After the first expression: if `for IDENT in ...`, this is
        // a comprehension. Otherwise it's a regular list — wrap as
        // LIST and continue collecting the rest.
        if self.at(SyntaxKind::IDENT) && self.current_text() == Some("for") {
            self.open_at(outer_ck, SyntaxKind::COMPREHENSION);
            self.bump(); // `for`
            if self.at(SyntaxKind::IDENT) {
                self.bump();
            } else {
                self.error_at_current("expected identifier after `for`");
            }
            if self.at(SyntaxKind::IDENT) && self.current_text() == Some("in") {
                self.bump();
            } else {
                self.error("expected `in` in comprehension");
            }
            self.parse_expr();
            if self.at(SyntaxKind::IDENT) && self.current_text() == Some("if") {
                self.bump();
                self.parse_expr();
            }
            self.expect(SyntaxKind::R_BRACK);
            self.close();
            return;
        }
        // Regular list — wrap the existing first element into a LIST
        // node and continue.
        self.open_at(outer_ck, SyntaxKind::LIST);
        if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_BRACK) {
            self.error_recover(
                "expected `,` or `]` in list",
                &[SyntaxKind::COMMA, SyntaxKind::R_BRACK],
            );
            self.eat(SyntaxKind::COMMA);
        }
        self.parse_list_body_tail();
    }

    /// Consume the remainder of a LIST body (after the optional leading
    /// element + comma have already been emitted) up to and including
    /// the closing `]`, then close the LIST node.
    fn parse_list_body_tail(&mut self) {
        while !self.at(SyntaxKind::R_BRACK) && !self.at_end() {
            self.parse_expr();
            if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_BRACK) {
                self.error_recover(
                    "expected `,` or `]` in list",
                    &[SyntaxKind::COMMA, SyntaxKind::R_BRACK],
                );
                self.eat(SyntaxKind::COMMA);
            }
        }
        self.expect(SyntaxKind::R_BRACK);
        self.close();
    }

    /// Text of the current (non-trivia) token, or None at EOI. Used by
    /// keyword-tail productions (`for`, `in`, `if`, `match`, `where`,
    /// `with`) that the lexer doesn't split out.
    fn current_text(&self) -> Option<&'a str> {
        let idx = self.pos_skip_trivia();
        self.tokens.get(idx).map(|(_, t)| *t)
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
        // Attribute-only field: `#import x from "p", "next": 1` — the
        // `#import` directive already consumed its full body, leaving
        // the field separator next. Same for a sequence of bare
        // directives whose payload is the field itself (e.g.
        // `#schema X { ... },`). Close the field here so the surrounding
        // dict resumes at the separator.
        if matches!(
            self.current(),
            Some(SyntaxKind::COMMA) | Some(SyntaxKind::R_BRACE)
        ) {
            self.close();
            return;
        }
        // The key: an ident, a string, or `...` (spread).
        if self.at(SyntaxKind::ELLIPSIS) {
            self.open(SyntaxKind::SPREAD_EXPR);
            self.bump();
            // v1.3 typed spread `...<Type> source` — same shape as the
            // atom-level spread, but here we sit inside a dict field
            // so the source expression can be a richer form.
            if self.at(SyntaxKind::LT) {
                self.bump();
                self.parse_type();
                self.expect(SyntaxKind::GT);
            }
            self.parse_expr();
            self.close();
            self.close();
            return;
        }
        // Optional leading type hint: `Type key: value` /
        // `Type key(params): body`. We commit only when peeking
        // suggests a typed-key shape — otherwise the leading run is
        // the key itself (e.g. a single identifier). v1.7 tuple types
        // (`(Int, String) pair: ...`) take the same slot and are
        // detected by a separate `(...)`-leading peek.
        if self.peek_is_tuple_typed_dict_key() {
            self.parse_tuple_type();
        } else if self.peek_is_typed_dict_key() {
            self.parse_type();
        }
        if self.at(SyntaxKind::IDENT) || self.at(SyntaxKind::STRING) {
            self.bump();
        } else if self.at(SyntaxKind::L_BRACK) {
            // Dynamic key `[expr]: value`.
            self.bump();
            // Optional `<T>` type-hint between `[` and the expression.
            if self.at(SyntaxKind::LT) {
                self.bump();
                self.parse_type();
                self.expect(SyntaxKind::GT);
            }
            self.parse_expr();
            self.expect(SyntaxKind::R_BRACK);
        } else {
            self.error_recover(
                "expected dict key",
                &[SyntaxKind::COLON, SyntaxKind::COMMA, SyntaxKind::R_BRACE],
            );
        }
        // Method-shorthand closure: `key(params) [-> Ret]: body`.
        // Detect via a `(` immediately after the key. We commit to the
        // closure interpretation whenever a `(` follows the key, since
        // the v1 grammar already reserves that position exclusively
        // for the method shorthand.
        if self.at(SyntaxKind::L_PAREN) {
            // Emit `(params) [-> Ret]` as a CLOSURE_PARAM list now;
            // the body that follows the `:` will be wrapped together
            // with the params into a CLOSURE node via a checkpoint.
            let closure_ck = self.checkpoint();
            self.bump(); // (
            while !self.at(SyntaxKind::R_PAREN) && !self.at_end() {
                self.parse_closure_param();
                if !self.eat(SyntaxKind::COMMA) && !self.at(SyntaxKind::R_PAREN) {
                    self.error_recover(
                        "expected `,` or `)` in closure parameter list",
                        &[SyntaxKind::COMMA, SyntaxKind::R_PAREN],
                    );
                    self.eat(SyntaxKind::COMMA);
                }
            }
            self.expect(SyntaxKind::R_PAREN);
            // Optional `-> RetType`.
            if self.eat(SyntaxKind::THIN_ARROW) {
                self.parse_type();
            }
            if self.eat(SyntaxKind::COLON) {
                self.open_at(closure_ck, SyntaxKind::CLOSURE);
                self.parse_expr();
                self.close();
            } else {
                self.error("expected `:` in dict field");
            }
        } else if self.eat(SyntaxKind::COLON) {
            self.parse_expr();
        } else {
            self.error("expected `:` in dict field");
        }
        self.close();
    }

    /// Does the upcoming token stream start with a Type-shaped run
    /// followed by an IDENT (or STRING) and then `:` / `(` (i.e. a
    /// typed-dict-key, NOT a dotted-path or a bare key)? Conservative
    /// — false negatives are fine (the field still parses untyped),
    /// but a false positive would consume the key as a type.
    fn peek_is_typed_dict_key(&self) -> bool {
        // Same logic as peek_is_typed_param, but we also accept STRING
        // as the trailing key segment, and we require a following
        // `:` or `(` so a dotted-path-as-value doesn't trip us up.
        if !self.at(SyntaxKind::IDENT) {
            return false;
        }
        let mut idx = self.pos_skip_trivia() + 1;
        let advance_trivia = |i: &mut usize, toks: &[(SyntaxKind, &str)]| {
            while *i < toks.len() && toks[*i].0.is_trivia() {
                *i += 1;
            }
        };
        advance_trivia(&mut idx, &self.tokens);
        while idx < self.tokens.len() && self.tokens[idx].0 == SyntaxKind::DOT {
            idx += 1;
            advance_trivia(&mut idx, &self.tokens);
            if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::IDENT) {
                idx += 1;
                advance_trivia(&mut idx, &self.tokens);
            } else {
                return false;
            }
        }
        let saw_generics = self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::LT);
        if saw_generics {
            let mut depth: i32 = 1;
            // Track nested `(` / `)` so a tuple-type argument like
            // `List<(Int, String)>` doesn't make the rejection bail
            // out the moment it hits a comma or `)`.
            let mut paren_depth: i32 = 0;
            idx += 1;
            while idx < self.tokens.len() && depth > 0 {
                match self.tokens[idx].0 {
                    SyntaxKind::LT => depth += 1,
                    SyntaxKind::GT => depth -= 1,
                    SyntaxKind::L_PAREN => paren_depth += 1,
                    SyntaxKind::R_PAREN if paren_depth > 0 => paren_depth -= 1,
                    SyntaxKind::L_BRACE
                    | SyntaxKind::R_BRACE
                    | SyntaxKind::R_PAREN
                    | SyntaxKind::FAT_ARROW
                    | SyntaxKind::THIN_ARROW
                    | SyntaxKind::COLON
                        if depth == 1 && paren_depth == 0 =>
                    {
                        return false
                    }
                    _ => {}
                }
                idx += 1;
            }
            if depth != 0 {
                return false;
            }
            advance_trivia(&mut idx, &self.tokens);
        }
        let saw_question = self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::QUESTION);
        if saw_question {
            idx += 1;
            advance_trivia(&mut idx, &self.tokens);
        }
        // Now we must see IDENT or STRING (the key) followed by `:`
        // or `(`. If neither, the leading run wasn't a type — bail
        // and let the surrounding parser treat it as the key itself.
        if !matches!(
            self.tokens.get(idx).map(|(k, _)| *k),
            Some(SyntaxKind::IDENT) | Some(SyntaxKind::STRING)
        ) {
            return false;
        }
        let mut after_key = idx + 1;
        advance_trivia(&mut after_key, &self.tokens);
        let next = self.tokens.get(after_key).map(|(k, _)| *k);
        matches!(next, Some(SyntaxKind::COLON) | Some(SyntaxKind::L_PAREN))
    }

    /// Does the upcoming token stream start with a balanced `(...)`
    /// tuple-type prefix followed by an IDENT (or STRING) and then
    /// `:` / `(` (i.e. `(Int, String) pair: ...`)? Used by
    /// [`parse_dict_field`] to commit to the tuple-type lead, which
    /// has to win over the "parens group" interpretation of the same
    /// bytes when they appear at the head of a dict field. The
    /// balanced paren scan walks past nested generics / nested parens
    /// so `List<(Int, String)>` doesn't fool the outer detector.
    fn peek_is_tuple_typed_dict_key(&self) -> bool {
        if !self.at(SyntaxKind::L_PAREN) {
            return false;
        }
        let lparen_idx = self.pos_skip_trivia();
        let Some(after_paren) = self.scan_after_matching_paren(lparen_idx) else {
            return false;
        };
        // Optional trailing `?` after the tuple type.
        let mut idx = after_paren;
        if self.tokens.get(idx).map(|(k, _)| *k) == Some(SyntaxKind::QUESTION) {
            idx += 1;
            while idx < self.tokens.len() && self.tokens[idx].0.is_trivia() {
                idx += 1;
            }
        }
        // Must see IDENT or STRING (the key), followed by `:` or `(`.
        if !matches!(
            self.tokens.get(idx).map(|(k, _)| *k),
            Some(SyntaxKind::IDENT) | Some(SyntaxKind::STRING)
        ) {
            return false;
        }
        let mut after_key = idx + 1;
        while after_key < self.tokens.len() && self.tokens[after_key].0.is_trivia() {
            after_key += 1;
        }
        matches!(
            self.tokens.get(after_key).map(|(k, _)| *k),
            Some(SyntaxKind::COLON) | Some(SyntaxKind::L_PAREN)
        )
    }

    fn parse_call_args(&mut self) {
        self.open(SyntaxKind::CALL_ARG);
        self.bump(); // (
        while !self.at(SyntaxKind::R_PAREN) && !self.at_end() {
            self.parse_call_arg();
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

    /// One argument inside a call's parens. Either positional (a
    /// bare expression) or named (`IDENT = expression`). The latter
    /// is detected by peeking IDENT-followed-by-EQ — the legacy
    /// `parse_call_arg` (`fn_call.rs`) uses the same lookahead. We
    /// emit the IDENT + EQ + value expression as siblings of each
    /// other under the parent CALL_ARG node so the lowering pass can
    /// pick the name back out without re-running token logic.
    fn parse_call_arg(&mut self) {
        if self.at(SyntaxKind::IDENT) && self.nth(1) == Some(SyntaxKind::EQ) {
            // Named: IDENT EQ <expr>.
            self.bump(); // name
            self.bump(); // =
            self.parse_expr();
        } else {
            self.parse_expr();
        }
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

/// Body shape every known `#name` directive expects. Mirrors the
/// legacy `directive::DIRECTIVE_SHAPES` table (in `directive.rs`) —
/// the lossless CST grammar takes the body bytes verbatim, but it
/// still has to know whether to *try* to consume one (Value /
/// NameBody / Import / Main) or stop right after the keyword (Bare).
/// Unknown names fall through to `Value` so the corpus's permissive
/// behaviour around third-party-looking directives is preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectiveShape {
    Bare,
    Value,
    NameBody,
    Import,
    Main,
}

/// Map a directive name to its expected body shape. Keep in sync with
/// `crate::directive::DIRECTIVE_SHAPES`.
fn directive_shape(name: &str) -> DirectiveShape {
    match name {
        "private" | "strict" | "native" => DirectiveShape::Bare,
        "default" | "expect" | "msg" | "error" | "brand" | "derive" | "no_auto_derive" => {
            DirectiveShape::Value
        }
        "schema" | "extend" => DirectiveShape::NameBody,
        "import" => DirectiveShape::Import,
        "main" => DirectiveShape::Main,
        _ => DirectiveShape::Value,
    }
}

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
        parse_round_trip("{\n    foo: [1, 2, 3],\n    bar: { baz: \"hi\" }\n}\n");
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
    fn method_shorthand_emits_closure() {
        let parsed = parse_round_trip("{ add(a, b): a + b }");
        assert!(!parsed.has_errors());
        let closures: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::CLOSURE)
            .collect();
        assert_eq!(closures.len(), 1, "expected exactly one CLOSURE node");
        let params: Vec<_> = closures[0]
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::CLOSURE_PARAM)
            .collect();
        assert_eq!(params.len(), 2, "expected two CLOSURE_PARAMs");
    }

    #[test]
    fn standalone_paren_closure() {
        let parsed = parse_round_trip("{ f: (a, b) => a + b }");
        assert!(!parsed.has_errors());
        let closures: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::CLOSURE)
            .collect();
        assert_eq!(closures.len(), 1);
    }

    #[test]
    fn list_comprehension_emits_comprehension_node() {
        let parsed = parse_round_trip("{ xs: [x * 2 for x in src if x > 0] }");
        assert!(!parsed.has_errors());
        let comps: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::COMPREHENSION)
            .collect();
        assert_eq!(comps.len(), 1);
        // The COMPREHENSION should NOT also be a LIST.
        let lists: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::LIST)
            .collect();
        // The dict body is not a list, so the only [...] in source
        // becomes a COMPREHENSION — no LIST nodes at top level.
        assert!(
            lists.is_empty(),
            "comprehension `[...]` should not also produce a LIST"
        );
    }

    #[test]
    fn match_expression_emits_match_node() {
        let parsed = parse_round_trip(
            "{ render(item): item match { Image: \"i\", Text: \"t\", * : \"u\" } }",
        );
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let matches: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::MATCH_EXPR)
            .collect();
        assert_eq!(matches.len(), 1);
        let arms: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::MATCH_ARM)
            .collect();
        assert_eq!(arms.len(), 3);
    }

    #[test]
    fn schema_directive_with_body() {
        let parsed = parse_round_trip("#schema User { String name: *, Int age: * }\n{ a: 1 }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let dirs: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::DIRECTIVE)
            .collect();
        assert_eq!(dirs.len(), 1);
    }

    #[test]
    fn schema_with_generic_params_and_with_block() {
        let parsed = parse_round_trip(
            "#schema Result<T, E> { T value: *, E error: * } with { unwrap(): value }\n{ x: 1 }",
        );
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn import_directive_round_trip() {
        let parsed = parse_round_trip("#import string from \"std/string\"\n{ x: 1 }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn main_directive_round_trip() {
        let parsed = parse_round_trip("#main(User u, Cart cart) -> Result<Order>\n{ x: 1 }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn f_string_emits_f_string_node() {
        let parsed = parse_round_trip(r#"{ msg: f"hello ${name}!" }"#);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let fs: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::F_STRING)
            .collect();
        assert_eq!(fs.len(), 1);
        let interps: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::F_STRING_INTERPOLATION)
            .collect();
        assert_eq!(interps.len(), 1);
        // Interpolation body should contain a VARIABLE_EXPR for `name`.
        let interp = &interps[0];
        let vars: Vec<_> = interp
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::VARIABLE_EXPR)
            .collect();
        assert!(!vars.is_empty(), "expected VARIABLE_EXPR inside interp");
    }

    #[test]
    fn raw_f_string_round_trip() {
        parse_round_trip("{ msg: f#\"raw ${x} text\"# }");
    }

    #[test]
    fn plain_string_still_literal() {
        let parsed = parse_round_trip(r#"{ x: "hi" }"#);
        let fs: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::F_STRING)
            .collect();
        assert!(fs.is_empty(), "plain string should not be F_STRING");
    }

    #[test]
    fn where_expression_emits_where_node() {
        let parsed = parse_round_trip("{ x: a + b where { a: 1, b: 2 } }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let wheres: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::WHERE_EXPR)
            .collect();
        assert_eq!(wheres.len(), 1);
    }

    #[test]
    fn list_without_for_stays_list() {
        let parsed = parse_round_trip("{ xs: [1, 2, 3] }");
        assert!(!parsed.has_errors());
        let lists: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::LIST)
            .collect();
        assert_eq!(lists.len(), 1);
    }

    #[test]
    fn generic_type_in_closure_param() {
        let parsed = parse_round_trip("{ extract(List<Int> xs, String? sep): xs }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let types: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TYPE_NODE)
            .collect();
        // `List<Int>` outer + `Int` nested + `String?` = 3 TYPE_NODEs.
        assert!(
            types.len() >= 3,
            "expected at least 3 TYPE_NODE, got {}",
            types.len()
        );
    }

    #[test]
    fn comparison_lt_not_treated_as_generics() {
        // The closure-param peek must NOT decide `a < b` is a typed
        // param — there's whitespace between `a` and `<`. The dict
        // body should be a single binary expression.
        let parsed = parse_round_trip("{ f: a < b }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let binaries: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::BINARY_EXPR)
            .collect();
        assert_eq!(binaries.len(), 1, "expected one BINARY_EXPR");
    }

    #[test]
    fn typed_closure_param_records_type_node() {
        let parsed = parse_round_trip("{ add(Int a, Int b): a + b }");
        assert!(!parsed.has_errors());
        let type_nodes: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TYPE_NODE)
            .collect();
        assert!(
            type_nodes.len() >= 2,
            "expected TYPE_NODEs for typed params"
        );
    }

    #[test]
    fn comments_round_trip() {
        parse_round_trip("// header\n{\n    // inner\n    x: 1, /* trail */ y: 2\n}\n");
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

    #[test]
    fn variant_ctor_emits_variant_node() {
        let parsed = parse_round_trip("{ x: Result.Ok { value: 1 } }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let vc: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::VARIANT_CTOR)
            .collect();
        assert_eq!(vc.len(), 1);
    }

    #[test]
    fn variant_ctor_three_segment_path() {
        let parsed = parse_round_trip("{ x: Foo.Bar.Baz { field: 1 } }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let vc: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::VARIANT_CTOR)
            .collect();
        assert_eq!(vc.len(), 1);
    }

    #[test]
    fn dotted_access_without_brace_stays_variable() {
        // `foo.bar` alone is member access — must NOT become a
        // VARIANT_CTOR. Walks the post-fix path the same as before.
        let parsed = parse_round_trip("{ x: foo.bar }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let vc: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::VARIANT_CTOR)
            .collect();
        assert!(vc.is_empty(), "single dotted access should not be a ctor");
    }

    #[test]
    fn named_call_args_parse_without_errors() {
        let parsed = parse_round_trip("{ y: map(f = g) }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        // The CALL_ARG node contains the IDENT, EQ, and value side by
        // side; the lowering pass groups them back into a `CallArg`.
        let call_args: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::CALL_ARG)
            .collect();
        assert_eq!(call_args.len(), 1);
        let has_eq = call_args[0]
            .children_with_tokens()
            .filter_map(|el| el.into_token())
            .any(|t| t.kind() == SyntaxKind::EQ);
        assert!(has_eq, "named arg should carry an EQ token");
    }

    #[test]
    fn mixed_positional_and_named_args() {
        let parsed = parse_round_trip("{ z: f(1, name = expr, more = 2) }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn ternary_expression_emits_ternary_node() {
        let parsed = parse_round_trip("{ x: a ? 1 : 2 }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let ts: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TERNARY_EXPR)
            .collect();
        assert_eq!(ts.len(), 1, "expected one TERNARY_EXPR");
    }

    #[test]
    fn ternary_root_no_whitespace() {
        // Legacy accepts `true? 1:2` — every `?` / `:` boundary is
        // surrounded by `soc0` so adjacent forms parse without spaces.
        let parsed = parse_round_trip("true? 1:2");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn ternary_nested_in_else() {
        // Right-recursive parse: `a ? 1 : b ? 2 : 3` should produce a
        // ternary whose `els` is another ternary.
        let parsed = parse_round_trip("{ x: a ? 1 : b ? 2 : 3 }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let ts: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TERNARY_EXPR)
            .collect();
        assert_eq!(ts.len(), 2);
    }

    #[test]
    fn bare_directive_does_not_consume_next_field() {
        // `#private` is a bare directive; the IDENT after it must
        // belong to the next dict field, not to the directive body.
        let src = "{ #private\n  field(s): s, next: 1 }";
        let parsed = parse_round_trip(src);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn dict_field_can_be_attribute_only() {
        // `#import x from "p"` consumes its whole body; the field is
        // attribute-only and the `,` belongs to the surrounding dict.
        let src = "{ #import x from \"p\", next: 1 }";
        let parsed = parse_round_trip(src);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn schema_with_block_emits_method_nodes() {
        // Slice-opener for the schema with-block grammar. Two methods
        // back-to-back, one carrying a `#derive` pragma and a `Self`
        // parameter type.
        let src = "#schema Money { Int cents: * } with {\n    #derive Equatable\n    eq(other: Self) -> Bool: self.cents == other.cents\n}\n{ Money p: { cents: 100 } }\n";
        let parsed = parse_round_trip(src);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let with_blocks: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SCHEMA_WITH)
            .collect();
        assert_eq!(with_blocks.len(), 1);
        let methods: Vec<_> = with_blocks[0]
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SCHEMA_METHOD)
            .collect();
        assert_eq!(methods.len(), 1);
        // The method should contain the `#derive` directive and a
        // CLOSURE_PARAM for `other`.
        let dirs: Vec<_> = methods[0]
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::DIRECTIVE)
            .collect();
        assert_eq!(dirs.len(), 1);
        let params: Vec<_> = methods[0]
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::CLOSURE_PARAM)
            .collect();
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn schema_with_block_native_method_skips_body() {
        // `#native` method has no `: body` — just the signature.
        let src =
            "#schema Doc { String text: * } with {\n    #native\n    render() -> String\n}\n{}\n";
        let parsed = parse_round_trip(src);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn tuple_index_access_round_trips() {
        // v1.8 positional access `xs.0` — number after the dot is a
        // valid path tail, alongside identifier-style `xs.field`.
        let parsed = parse_round_trip("{ Int head: xs.0 }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn type_atom_for_brand_directive_body() {
        // `#brand Dict<String, Int> { ... }` — the brand directive's
        // body is a type-tagged dict. The leading IDENT `Dict` (a
        // known type head) must lower into a TYPE_NODE so the
        // generics aren't mistaken for binary `<` / `>` operators.
        let src = "{ counters: #brand Dict<String, Int> { hits: 1 } }";
        let parsed = parse_round_trip(src);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let types: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TYPE_NODE)
            .collect();
        assert!(!types.is_empty(), "expected a TYPE_NODE for Dict<...>");
    }

    #[test]
    fn enum_with_struct_variant_inside_generic_args() {
        // v1.8 Phase C: sum-type generics admit a struct-variant body
        // (`Enum<Variant { field: Type }>`) as one of the generic
        // arguments. Round-trip captures the inner `{ ... }` as a
        // child of the outer TYPE_NODE.
        let src = "#schema Pair<T, U> Enum<Both { left: T, right: U }>\n{}\n";
        let parsed = parse_round_trip(src);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn typed_spread_round_trips() {
        // v1.3 typed spread `...<Type> expr`. The `<Type>` annotation
        // lands inside the SPREAD_EXPR; the source expression follows.
        let parsed = parse_round_trip("{ val: { ...<Extra> base } }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let spreads: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::SPREAD_EXPR)
            .collect();
        assert_eq!(spreads.len(), 1, "expected one SPREAD_EXPR");
        let types: Vec<_> = spreads[0]
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TYPE_NODE)
            .collect();
        assert!(!types.is_empty(), "typed spread should carry a TYPE_NODE");
    }

    #[test]
    fn tuple_type_in_dict_field_round_trips() {
        // v1.7 tuple types in the type-hint slot of a dict field.
        let parsed = parse_round_trip("{ (Int, String) pair: [42, \"hello\"] }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let tts: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TUPLE_TYPE)
            .collect();
        assert_eq!(tts.len(), 1, "expected one TUPLE_TYPE");
    }

    #[test]
    fn tuple_type_inside_generic() {
        let parsed = parse_round_trip("{ List<(Int, String)> rows: [[1, \"a\"]] }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
        let tts: Vec<_> = parsed
            .syntax()
            .descendants()
            .filter(|n| n.kind() == SyntaxKind::TUPLE_TYPE)
            .collect();
        assert_eq!(tts.len(), 1);
    }

    #[test]
    fn tuple_type_zero_and_one() {
        // Zero-tuple `()` and one-tuple `(T,)` both round-trip
        // cleanly. The trailing comma in the one-tuple matters for the
        // typed-AST layer (it disambiguates from `(T)` parens), but the
        // CST keeps the bytes verbatim.
        let parsed = parse_round_trip("{ () unit: [], (Int,) one: [1] }");
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    #[test]
    fn decorator_dotted_name_round_trips() {
        // `@ensure.int` / `@ensure.at_least(1024)` — dotted decorator
        // names appear in the corpus alongside plain `@name(...)`.
        let src = "{ @ensure.int\n  @ensure.at_least(1024)\n  \"port\": 80 }";
        let parsed = parse_round_trip(src);
        assert!(!parsed.has_errors(), "errors: {:?}", parsed.errors);
    }

    /// Monotonic floor on how many checked-in `.relon` fixtures parse
    /// without ANY ERROR nodes. Each P2 slice MUST raise this number;
    /// regressions need a deliberate, recorded reason.
    ///
    /// The floor starts at 30 (closures slice). Bump it as more P2
    /// grammar lands.
    #[test]
    fn fixtures_clean_parse_floor() {
        // Each P2 slice bumps the floor. At slice 1 (closures) we hit
        // ~60 of ~210 — the directive / match / where / type slices
        // pushed this to 135. After the P4-prep grammar gaps
        // (ternary / named call args / variant ctor) we reach 148.
        // Directive-shape dispatch + attribute-only dict fields pushed
        // it to 157 (the next P2 slices target tuple types, typed
        // spreads, and the schema with-block named-param method
        // grammar). Tuple types `(T1, T2)` brought the floor to 165.
        // Typed spreads `...<Type> expr` brought it to 170.
        // Schema with-block structured method nodes brought it to 198.
        // Tuple-index `.N` access, type-atom recognition for
        // `#brand Dict<K, V> { ... }` / `Weather? { ... }`,
        // Enum-with-struct-variant inside generic args, and
        // expression-level leading attributes brought it to 208.
        // The remaining two `.relon` files
        // (`with_block_invalid/*.relon`) are intentional parse-error
        // fixtures used by the legacy parser's negative test suite.
        const FLOOR: usize = 208;
        let clean = fixture_clean_parse_count();
        eprintln!("[parser] fixtures clean-parse count: {clean}");
        assert!(
            clean >= FLOOR,
            "regressed clean-parse count: floor={FLOOR}, actual={clean}",
        );
    }

    fn fixture_clean_parse_count() -> usize {
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
        let mut clean = 0usize;
        for path in files {
            let source = fs::read_to_string(&path).unwrap_or_default();
            if source.is_empty() {
                continue;
            }
            let parsed = parse_cst(&source);
            if !parsed.has_errors() {
                clean += 1;
            }
        }
        clean
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
            let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let parsed = parse_cst(&source);
            let reconstructed = parsed.syntax().text().to_string();
            assert_eq!(reconstructed, source, "round-trip mismatch on {path:?}");
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
