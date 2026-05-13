#![forbid(unsafe_code)]

use relon_parser::{
    parse_document,
    source::{tokenize_source, SourceToken as Token, SourceTokenKind as TokenKind},
};
use std::path::PathBuf;

const INDENT: &str = "    ";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("parse error: {0}")]
    Parse(String),

    #[error("tokenize error: {0}")]
    Tokenize(String),

    #[error("format check failed")]
    CheckFailed,

    #[error("{0}")]
    Usage(String),
}

pub fn format_source(source: &str) -> Result<String, Error> {
    validate_source(source)?;
    let tokens = tokenize_source(source).map_err(|error| Error::Tokenize(error.to_string()))?;
    let mut formatter = SourceFormatter::new(&tokens);
    let output = formatter.format();
    validate_source(&output)?;
    Ok(output)
}

pub fn is_formatted(source: &str) -> Result<bool, Error> {
    Ok(format_source(source)? == source)
}

fn validate_source(source: &str) -> Result<(), Error> {
    parse_document(source).map_err(|error| Error::Parse(error.to_string()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Frame {
    Brace,
    Bracket,
    Paren,
    Index,
}

struct SourceFormatter<'a> {
    tokens: &'a [Token<'a>],
    index: usize,
    output: String,
    indent: usize,
    line_start: bool,
    frames: Vec<Frame>,
    previous: Option<Token<'a>>,
    type_generic_depth: usize,
    /// Per-Brace-frame tracker for method/field pair classification.
    /// Mirrors `frames` for Brace entries — pushed on `{` and popped
    /// on `}`. Drives the once-per-Dict blank line that separates
    /// the leading method group (`name(params): body`) from the
    /// trailing field group (`name: value`).
    pair_class_stack: Vec<PairFrameTracker>,
}

#[derive(Debug, Default)]
struct PairFrameTracker {
    /// Class of the previous pair emitted in this Dict, used to
    /// detect a method→field transition. `None` until the first
    /// pair has been classified.
    prev_class: Option<PairClass>,
    /// True once we've already emitted the method→field separator
    /// for this Dict — ensures we only paragraph-break once even if
    /// pairs continue to alternate.
    transition_emitted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairClass {
    /// Closure-bound pair: `name(params): body` — the key is
    /// followed by `(` (open paren of the param list).
    Method,
    /// Value-bound pair: `name: value`, `"key": value`, or a typed
    /// schema field `Type name: ...`. Any pair whose key isn't
    /// followed by `(`.
    Field,
}

impl<'a> SourceFormatter<'a> {
    fn new(tokens: &'a [Token<'a>]) -> Self {
        Self {
            tokens,
            index: 0,
            output: String::new(),
            indent: 0,
            line_start: true,
            frames: Vec::new(),
            previous: None,
            type_generic_depth: 0,
            pair_class_stack: Vec::new(),
        }
    }

    fn format(&mut self) -> String {
        while self.index < self.tokens.len() {
            let token = self.tokens[self.index];
            let effective = self.format_token(token);
            self.previous = effective.map(|kind| Token { kind, ..token });
            self.index += 1;
        }

        self.trim_trailing_spaces();
        while self.output.ends_with('\n') {
            self.output.pop();
        }
        self.output.push('\n');
        std::mem::take(&mut self.output)
    }

    fn format_token(&mut self, token: Token<'a>) -> Option<TokenKind> {
        match token.kind {
            TokenKind::LineComment => {
                self.format_line_comment(token);
                Some(TokenKind::LineComment)
            }
            TokenKind::BlockComment => {
                self.format_block_comment(token);
                Some(TokenKind::BlockComment)
            }
            _ => {
                self.apply_leading_newline(token);
                match token.kind {
                    TokenKind::OpenBrace => {
                        Some(self.format_open_multiline(token, TokenKind::CloseBrace, Frame::Brace))
                    }
                    TokenKind::CloseBrace => {
                        self.format_close_multiline("}", Frame::Brace);
                        Some(TokenKind::CloseBrace)
                    }
                    TokenKind::OpenBracket if self.is_path_index(token) => {
                        self.write_plain("[");
                        self.frames.push(Frame::Index);
                        Some(TokenKind::OpenBracket)
                    }
                    TokenKind::OpenBracket => Some(self.format_open_multiline(
                        token,
                        TokenKind::CloseBracket,
                        Frame::Bracket,
                    )),
                    TokenKind::CloseBracket if self.top_frame() == Some(Frame::Index) => {
                        self.write_plain("]");
                        self.frames.pop();
                        Some(TokenKind::CloseBracket)
                    }
                    TokenKind::CloseBracket => {
                        self.format_close_multiline("]", Frame::Bracket);
                        Some(TokenKind::CloseBracket)
                    }
                    TokenKind::OpenParen => {
                        self.write_plain("(");
                        self.frames.push(Frame::Paren);
                        Some(TokenKind::OpenParen)
                    }
                    TokenKind::CloseParen => {
                        self.write_plain(")");
                        self.pop_frame(Frame::Paren);
                        Some(TokenKind::CloseParen)
                    }
                    TokenKind::Comma => {
                        self.format_comma();
                        Some(TokenKind::Comma)
                    }
                    TokenKind::Colon => {
                        self.write_plain(":");
                        self.space();
                        Some(TokenKind::Colon)
                    }
                    TokenKind::Dot => {
                        self.write_plain(".");
                        Some(TokenKind::Dot)
                    }
                    TokenKind::At => {
                        self.write_value_prefix();
                        self.write_plain("@");
                        Some(TokenKind::At)
                    }
                    TokenKind::Hash => {
                        if self.is_top_level_block_directive() {
                            self.ensure_blank_line_separator();
                        }
                        self.write_value_prefix();
                        self.write_plain("#");
                        Some(TokenKind::Hash)
                    }
                    TokenKind::Amp => {
                        self.write_value_prefix();
                        self.write_plain("&");
                        Some(TokenKind::Amp)
                    }
                    TokenKind::Question => {
                        if self.is_type_optional(token) {
                            self.write_plain("?");
                            self.space_if_next_starts_value();
                        } else {
                            self.write_binary_operator("?");
                        }
                        Some(TokenKind::Question)
                    }
                    TokenKind::Ellipsis => {
                        self.write_value_prefix();
                        self.write_plain("...");
                        Some(TokenKind::Ellipsis)
                    }
                    TokenKind::Operator => {
                        if token.text == "<" && self.is_type_generic_open(token) {
                            self.write_plain("<");
                            self.type_generic_depth += 1;
                        } else if token.text == ">" && self.type_generic_depth > 0 {
                            self.write_plain(">");
                            self.type_generic_depth -= 1;
                            self.space_if_next_starts_value();
                        } else {
                            self.format_operator(token.text);
                        }
                        Some(TokenKind::Operator)
                    }
                    TokenKind::Equal => {
                        self.write_plain("=");
                        Some(TokenKind::Equal)
                    }
                    TokenKind::Word | TokenKind::Number | TokenKind::String => {
                        // Detect pair-start to apply the method→field
                        // group separator. Pair keys are Word / String
                        // tokens that arrive at line_start while
                        // we're directly inside a Brace frame (not in
                        // a type-generic).
                        if self.line_start
                            && self.top_frame() == Some(Frame::Brace)
                            && self.type_generic_depth == 0
                        {
                            self.maybe_paragraph_break_for_pair(token);
                        }
                        self.write_atom(token.text);
                        Some(token.kind)
                    }
                    TokenKind::LineComment | TokenKind::BlockComment => unreachable!(),
                }
            }
        }
    }

    fn format_open_multiline(
        &mut self,
        token: Token<'a>,
        close_kind: TokenKind,
        frame: Frame,
    ) -> TokenKind {
        // Blank line before a root-level `{` / `[` that follows a
        // directive body's closing brace — e.g. `#schema X { ... }`
        // and the file's value body. `#main(...) { ... }` is excluded
        // because `(` not `}` precedes the `{`. Without this, the
        // schema header and code body stick together as one block.
        if self.frames.is_empty()
            && self.previous.map(|p| p.kind) == Some(TokenKind::CloseBrace)
        {
            self.ensure_blank_line_separator();
        }
        self.write_value_prefix();

        if self.next_is(close_kind) {
            self.write_plain(match token.kind {
                TokenKind::OpenBrace => "{}",
                TokenKind::OpenBracket => "[]",
                _ => unreachable!(),
            });
            self.index += 1;
            return close_kind;
        }

        self.write_plain(token.text);
        self.frames.push(frame);
        if frame == Frame::Brace {
            self.pair_class_stack.push(PairFrameTracker::default());
        }
        self.indent += 1;
        self.newline();
        token.kind
    }

    fn format_close_multiline(&mut self, text: &str, frame: Frame) {
        self.pop_frame(frame);
        if frame == Frame::Brace {
            self.pair_class_stack.pop();
        }
        self.indent = self.indent.saturating_sub(1);
        if !self.line_start {
            self.newline();
        }
        self.write_indent();
        self.output.push_str(text);
        self.line_start = false;
    }

    fn format_comma(&mut self) {
        self.write_plain(",");
        if self.type_generic_depth > 0
            || self.next_is_inline_line_comment()
            || self.top_frame() == Some(Frame::Paren)
            || self.top_frame() == Some(Frame::Index)
        {
            self.space();
        } else {
            self.newline();
        }
    }

    /// `<` opens a type-generic (e.g. `Map<String, Int>`) when it directly
    /// follows an identifier token with no source whitespace, and is itself
    /// followed by another identifier. The heuristic intentionally rejects
    /// comparison forms like `a < b` (whitespace separates the tokens) and
    /// `a<10` (next token is a number, not an identifier).
    fn is_type_generic_open(&self, current: Token<'a>) -> bool {
        let Some(prev) = self.previous else {
            return false;
        };
        if prev.kind != TokenKind::Word {
            return false;
        }
        if current.start != prev.end {
            return false;
        }
        self.peek_next_non_trivia()
            .is_some_and(|t| t.kind == TokenKind::Word)
    }

    /// `?` marks a type as optional (e.g. `Foo?`, `Foo<X>?`) when it sits
    /// flush against the closing token of a type expression — an identifier
    /// or the `>` of a generic. With any whitespace before it the `?`
    /// belongs to a ternary and gets full binary spacing.
    fn is_type_optional(&self, current: Token<'a>) -> bool {
        let Some(prev) = self.previous else {
            return false;
        };
        let prev_closes_type =
            prev.kind == TokenKind::Word || (prev.kind == TokenKind::Operator && prev.text == ">");
        if !prev_closes_type {
            return false;
        }
        current.start == prev.end
    }

    fn peek_next_non_trivia(&self) -> Option<Token<'a>> {
        let mut i = self.index + 1;
        while i < self.tokens.len() {
            match self.tokens[i].kind {
                TokenKind::LineComment | TokenKind::BlockComment => i += 1,
                _ => return Some(self.tokens[i]),
            }
        }
        None
    }

    /// Emit a space if the next non-trivia token starts a value-shaped
    /// construct. Used to bridge `>` and `?` of a type expression to
    /// whatever follows (e.g. `Foo<X> field`, `Foo? field`); skips when
    /// the next token already includes its own leading layout (`,`,
    /// closing bracket, another `?`, etc.).
    fn space_if_next_starts_value(&mut self) {
        if let Some(next) = self.peek_next_non_trivia() {
            if matches!(
                next.kind,
                TokenKind::Word
                    | TokenKind::Number
                    | TokenKind::String
                    | TokenKind::OpenBrace
                    | TokenKind::OpenBracket
                    | TokenKind::At
                    | TokenKind::Hash
                    | TokenKind::Amp
                    | TokenKind::Ellipsis
            ) {
                self.space();
            }
        }
    }

    fn format_operator(&mut self, text: &str) {
        let unary = text == "!" || ((text == "-" || text == "+") && !self.previous_allows_binary());
        // `*` in value position (no preceding operand) is a Wildcard
        // placeholder (`String name: *`), not multiplication — emit
        // as a bare value so it doesn't pick up binary-operator
        // padding on either side.
        let wildcard = text == "*" && !self.previous_allows_binary();
        if unary || wildcard {
            self.write_value_prefix();
            self.write_plain(text);
        } else {
            self.write_binary_operator(text);
        }
    }

    fn format_line_comment(&mut self, token: Token<'a>) {
        if token.leading_newlines > 0 && !self.line_start {
            self.newline();
        }
        if self.line_start {
            self.write_indent();
        } else {
            self.space();
        }
        self.output.push_str(token.text.trim_end());
        self.newline();
    }

    fn format_block_comment(&mut self, token: Token<'a>) {
        if token.leading_newlines > 0 && !self.line_start {
            self.newline();
        }

        let was_line_start = self.line_start;
        if self.line_start {
            self.write_indent();
        } else {
            self.space();
        }

        self.output.push_str(token.text);
        self.line_start = token.text.ends_with('\n');

        if was_line_start || token.text.contains('\n') {
            self.newline();
        }
    }

    fn apply_leading_newline(&mut self, token: Token<'a>) {
        if self.line_start {
            return;
        }

        // Canonical layout: after a Dict-pair `:`, the value stays on
        // the same line as the key — IDE auto-format must be
        // deterministic, so we ignore the user's incoming whitespace
        // here. Multi-line values still wrap because they open a `{`
        // / `[` / `(` which has its own break behaviour.
        if self.previous.map(|p| p.kind) == Some(TokenKind::Colon)
            && self.top_frame() == Some(Frame::Brace)
            && !matches!(
                token.kind,
                TokenKind::OpenBrace | TokenKind::OpenBracket | TokenKind::OpenParen
            )
        {
            return;
        }

        if token.leading_newlines == 0 {
            return;
        }

        if matches!(
            token.kind,
            TokenKind::CloseBrace
                | TokenKind::CloseBracket
                | TokenKind::CloseParen
                | TokenKind::Comma
                | TokenKind::Colon
                | TokenKind::Dot
        ) {
            return;
        }

        if self.top_frame() == Some(Frame::Paren) || self.top_frame() == Some(Frame::Index) {
            return;
        }

        self.newline();
    }

    fn write_atom(&mut self, text: &str) {
        self.write_value_prefix();
        self.write_plain(text);
    }

    fn write_binary_operator(&mut self, text: &str) {
        self.space();
        self.write_plain(text);
        self.space();
    }

    fn write_value_prefix(&mut self) {
        if self.line_start {
            self.write_indent();
        } else if self.needs_space_before_value() {
            self.space();
        }
    }

    fn write_plain(&mut self, text: &str) {
        if self.line_start {
            self.write_indent();
        }
        self.output.push_str(text);
        self.line_start = text.ends_with('\n');
    }

    fn write_indent(&mut self) {
        if self.line_start {
            for _ in 0..self.indent {
                self.output.push_str(INDENT);
            }
            self.line_start = false;
        }
    }

    fn space(&mut self) {
        if !self.line_start && !self.output.ends_with([' ', '\n', '\t']) {
            self.output.push(' ');
        }
    }

    fn newline(&mut self) {
        self.trim_trailing_spaces();
        if !self.output.ends_with('\n') {
            self.output.push('\n');
        }
        self.line_start = true;
    }

    fn trim_trailing_spaces(&mut self) {
        while self.output.ends_with(' ') || self.output.ends_with('\t') {
            self.output.pop();
        }
    }

    fn next_is(&self, kind: TokenKind) -> bool {
        self.tokens
            .get(self.index + 1)
            .is_some_and(|token| token.kind == kind)
    }

    fn next_is_inline_line_comment(&self) -> bool {
        self.tokens.get(self.index + 1).is_some_and(|token| {
            token.kind == TokenKind::LineComment && token.leading_newlines == 0
        })
    }

    fn top_frame(&self) -> Option<Frame> {
        self.frames.last().copied()
    }

    fn pop_frame(&mut self, frame: Frame) {
        if self.top_frame() == Some(frame) {
            self.frames.pop();
        }
    }

    fn is_path_index(&self, token: Token<'a>) -> bool {
        token.leading_newlines == 0
            && matches!(
                self.previous.map(|token| token.kind),
                Some(TokenKind::Word)
                    | Some(TokenKind::Number)
                    | Some(TokenKind::String)
                    | Some(TokenKind::CloseBracket)
            )
    }

    /// Classify the upcoming pair (method vs field) by looking at
    /// the *post-key* token; if the previous pair was a method and
    /// this one is a field, insert a blank line so the two groups
    /// read as paragraphs. Fires at most once per Dict — once a
    /// transition has been emitted, subsequent pairs of either class
    /// stay flush.
    fn maybe_paragraph_break_for_pair(&mut self, key_token: Token<'a>) {
        let class = self.classify_pair_at(key_token);
        let Some(tracker) = self.pair_class_stack.last_mut() else {
            return;
        };
        let prev = tracker.prev_class;
        let already = tracker.transition_emitted;
        tracker.prev_class = Some(class);

        if !already && prev == Some(PairClass::Method) && class == PairClass::Field {
            tracker.transition_emitted = true;
            self.ensure_blank_line_separator();
        }
    }

    /// Inspect the token that follows the key to decide whether the
    /// pair is a method (`name(params): body`) or a field. Schema
    /// fields written as `Type name: ...` look like a Word followed
    /// by another Word — still classified as field because no `(`
    /// follows.
    fn classify_pair_at(&self, key_token: Token<'a>) -> PairClass {
        let next = self.tokens.get(self.index + 1);
        let Some(next) = next else {
            return PairClass::Field;
        };
        match (key_token.kind, next.kind) {
            (TokenKind::Word, TokenKind::OpenParen) => PairClass::Method,
            _ => PairClass::Field,
        }
    }

    /// True when the upcoming `#…` directive is one of the block-
    /// shaped forms at root scope: `#schema`, `#extend`, `#main`, or
    /// `#import`. These act as top-level "section headers" so we
    /// want a blank line ahead of them. Pair-level pragmas
    /// (`#private`, `#brand`, `#derive`, …) stay attached to their
    /// following pair and don't trigger the separator.
    fn is_top_level_block_directive(&self) -> bool {
        if !self.frames.is_empty() {
            return false;
        }
        let next = self.tokens.get(self.index + 1);
        let Some(next) = next else {
            return false;
        };
        if next.kind != TokenKind::Word {
            return false;
        }
        matches!(next.text, "schema" | "extend" | "main" | "import")
    }

    /// Emit a blank line before the next token if the output has
    /// already produced non-trivial content. Idempotent: subsequent
    /// calls collapse into a single blank line.
    fn ensure_blank_line_separator(&mut self) {
        if self.output.is_empty() {
            return;
        }
        // Already at line start: just check for the preceding blank.
        if !self.line_start {
            self.newline();
        }
        // Walk back over trailing newlines; we want exactly two
        // (`\n\n`) so the new token starts after one empty line.
        let trailing = self
            .output
            .chars()
            .rev()
            .take_while(|c| *c == '\n')
            .count();
        if trailing < 2 {
            for _ in trailing..2 {
                self.output.push('\n');
            }
        }
        self.line_start = true;
    }

    fn previous_allows_binary(&self) -> bool {
        matches!(
            self.previous.map(|token| token.kind),
            Some(TokenKind::Word)
                | Some(TokenKind::Number)
                | Some(TokenKind::String)
                | Some(TokenKind::CloseBrace)
                | Some(TokenKind::CloseBracket)
                | Some(TokenKind::CloseParen)
        )
    }

    fn needs_space_before_value(&self) -> bool {
        if self.output.ends_with([' ', '\n', '\t']) {
            return false;
        }

        matches!(
            self.previous.map(|token| token.kind),
            Some(TokenKind::Word)
                | Some(TokenKind::Number)
                | Some(TokenKind::String)
                | Some(TokenKind::CloseBrace)
                | Some(TokenKind::CloseBracket)
                | Some(TokenKind::CloseParen)
                | Some(TokenKind::LineComment)
                | Some(TokenKind::BlockComment)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_source() {
        let source = "{foo:1,bar:[2,3]}";
        let expected = "{\n    foo: 1,\n    bar: [\n        2,\n        3\n    ]\n}\n";

        assert_eq!(format_source(source).unwrap(), expected);
    }

    #[test]
    fn preserves_comments() {
        let source = "{\n// keep top\nfoo:1, // keep inline\nbar:{\n/* keep block */\nbaz:2\n}\n}";
        let expected = "{\n    // keep top\n    foo: 1, // keep inline\n    bar: {\n        /* keep block */\n        baz: 2\n    }\n}\n";

        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, expected);
        assert!(formatted.contains("// keep top"));
        assert!(formatted.contains("// keep inline"));
        assert!(formatted.contains("/* keep block */"));
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn preserves_string_contents() {
        let source = r###"{value:f"hello ${ call("x", /* not formatter trivia */ 1) }", raw:r#"// nope"#}"###;
        let formatted = format_source(source).unwrap();

        assert!(formatted.contains(r#"f"hello ${ call("x", /* not formatter trivia */ 1) }""#));
        assert!(formatted.contains(r##"r#"// nope"#"##));
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn checks_formatting() {
        let formatted = "{\n    foo: 1\n}\n";
        assert!(is_formatted(formatted).unwrap());
        assert!(!is_formatted("{foo:1}").unwrap());
    }

    #[test]
    fn rejects_trailing_tokens() {
        assert!(matches!(
            format_source("{} true"),
            Err(Error::Parse(message)) if message.contains("trailing input")
        ));
    }

    #[test]
    fn keeps_type_generics_compact() {
        // `<...>` adjacent to an identifier opens a type-generic; the
        // formatter must not pad the angle brackets like comparison
        // operators. Nested generics and dotted heads stay flush.
        for source in [
            "{\n    Dict<String, Int> m: {\n        a: 1\n    }\n}\n",
            "{\n    Dict<String, List<Int>> m: {\n        a: [\n            1\n        ]\n    }\n}\n",
            "{\n    x: #brand Dict<String, Int> {\n        a: 1\n    }\n}\n",
        ] {
            let formatted = format_source(source).unwrap();
            assert_eq!(formatted, source, "input did not round-trip");
            assert_eq!(format_source(&formatted).unwrap(), formatted);
        }
    }

    #[test]
    fn keeps_type_optional_compact() {
        // `?` flush against an identifier or `>` is the optional-type
        // marker, not the start of a ternary — no surrounding spaces.
        for source in [
            "{\n    Weather? w: {\n        a: 1\n    }\n}\n",
            "{\n    x: #brand Weather? {\n        a: 1\n    }\n}\n",
            "{\n    x: #brand Dict<String, Int>? {\n        a: 1\n    }\n}\n",
        ] {
            let formatted = format_source(source).unwrap();
            assert_eq!(formatted, source, "input did not round-trip");
            assert_eq!(format_source(&formatted).unwrap(), formatted);
        }
    }

    #[test]
    fn ternary_question_keeps_binary_spacing() {
        // The `?` of a ternary sits between values with whitespace, so
        // the type-optional heuristic must back off and keep the
        // operator-style spacing intact.
        let source = "{\n    abs(x): x < 0 ? -x: x\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn comparison_lt_gt_unchanged() {
        // `a < b` (with whitespace) must remain a comparison — adjacent
        // numbers/expressions should not get reinterpreted as type
        // generics.
        let source = "{\n    cmp(a, b): a < b ? a: b\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn arrow_token_keeps_compact() {
        // `->` must round-trip as a single token. Until source.rs added it
        // to the multi-char operator list, the formatter split it into
        // `-` + `>` and the result failed to re-parse.
        let source = "#main(Int x) -> Int\n{\n    n: x\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_with_block_round_trip() {
        // Schema-method `with { ... }` block — the trait-bound system's
        // Phase A surface. Round-trip and idempotence check. Note the
        // blank line between the schema declaration and the file's
        // root value body (paragraph-break rule: any `}` followed by
        // a root-level `{` gets one empty line).
        let source = "#schema Money {\n    Int cents: *\n} with {\n    cents_value() -> Int: self.cents\n}\n\n{\n    Money price: {\n        cents: 100\n    }\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_with_block_derive_pragma() {
        // `#derive Equatable` stacked above the witness method.
        let source = "#schema Money {\n    Int cents: *\n} with {\n    #derive Equatable\n    eq(other: Self) -> Bool: self.cents == other.cents\n}\n\n{\n    Money price: {\n        cents: 100\n    }\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn closure_body_inline_idempotent() {
        // Function-definition body always inlines after the colon —
        // input whitespace doesn't matter, the output is canonical.
        let inline = "{\n    currency(val, symbol): val + \" \" + symbol,\n    multiply(a, b): a * b\n}\n";
        let multiline = "{\n    currency(val, symbol):\n        val + \" \" + symbol,\n    multiply(a, b):\n        a * b\n}\n";
        assert_eq!(format_source(inline).unwrap(), inline);
        assert_eq!(format_source(multiline).unwrap(), inline);
    }

    #[test]
    fn wildcard_star_no_binary_padding() {
        // `*` as a schema-field wildcard (`String name: *,`) must not
        // pick up binary-operator spacing — it isn't multiplication.
        let source = "#schema User {\n    String name: *,\n    Int age: (a) => a >= 0\n}\n\n{\n    x: 1\n}\n";
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("String name: *,"), "expected `*,` flush: {formatted}");
        assert!(!formatted.contains("* ,"));
    }

    #[test]
    fn block_directives_get_blank_separator() {
        // Two `#schema` blocks back-to-back, plus `#main` after — each
        // top-level block directive starts after a blank line.
        let source = "#schema A { Int x: * } #schema B { Int y: * } #main(A a){ z: 1 }";
        let formatted = format_source(source).unwrap();
        assert!(
            formatted.contains("}\n\n#schema B"),
            "missing blank between #schema A and #schema B: {formatted}"
        );
        assert!(
            formatted.contains("}\n\n#main("),
            "missing blank between #schema B and #main: {formatted}"
        );
    }

    #[test]
    fn method_field_group_separator_inside_dict() {
        // Methods (`name(p): body`) come before fields (`name: value`)
        // in the demo preset; the formatter inserts one blank line at
        // the boundary.
        let source = "{\n    multiply(a, b): a * b,\n    project: {\n        name: \"x\"\n    }\n}\n";
        let formatted = format_source(source).unwrap();
        assert!(
            formatted.contains("a * b,\n\n    project:"),
            "expected blank line between method group and field group: {formatted}"
        );
    }
}
