#![forbid(unsafe_code)]

use relon_parser::{
    parse_document,
    source::{tokenize_source, SourceToken as Token, SourceTokenKind as TokenKind},
    Expr, Node, TokenKey,
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
    let root = parse_document(source).map_err(|error| Error::Parse(error.to_string()))?;
    // Phase 2: lift methods (closure-bound pairs) to the front of any
    // Dict in which they trail a non-closure field. Performed as a
    // pre-pass over the *source string* so subsequent token-level
    // formatting sees a canonical pair order; the formatter itself
    // doesn't need to know about the reorder.
    let reordered = reorder_methods_first(source, &root);
    let source_for_fmt = reordered.as_deref().unwrap_or(source);
    let tokens =
        tokenize_source(source_for_fmt).map_err(|error| Error::Tokenize(error.to_string()))?;
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

// ----- Phase 2: method-first pair reordering ----------------------------

/// Pair classification used by the reorder pre-pass. Mirrors
/// `PairClass` lower down (used by the formatter for separator
/// emission) — kept as a separate enum so the two stay decoupled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairKind {
    /// Value is an `Expr::Closure { ... }` — i.e. a function
    /// definition (`name(p): body` lowered by the parser).
    Method,
    Field,
}

/// Single byte-range edit applied to the source string.
struct PairEdit {
    /// Inclusive start in the original source.
    start: usize,
    /// Exclusive end in the original source.
    end: usize,
    /// Replacement text for `source[start..end]`.
    replacement: String,
}

/// Top-level reorder driver. Walks the parsed AST, collects edits for
/// each Dict whose pairs are not already methods-first, then applies
/// the edits right-to-left to produce a new source string. Returns
/// `None` when no Dict needed reordering — the caller can then keep
/// the original source pointer.
fn reorder_methods_first(source: &str, root: &Node) -> Option<String> {
    let mut edits: Vec<PairEdit> = Vec::new();
    collect_dict_reorder_edits(root, false, source, &mut edits);
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = source.to_string();
    for edit in edits {
        out.replace_range(edit.start..edit.end, &edit.replacement);
    }
    Some(out)
}

/// Walk `node` bottom-up, queuing a reorder edit for any Dict whose
/// pairs need methods grouped to the front. `in_directive_body` is
/// `true` exactly when this node is the immediate body of a
/// `#schema` / `#extend` / `#derive` / similar `NameBody`/`Value`
/// directive — those Dicts encode declarations whose order is
/// semantically meaningful (field-declaration order), so we leave
/// them alone. Children of the body that happen to be Dicts again
/// (e.g. a nested default-value `{ ... }`) reorder normally.
fn collect_dict_reorder_edits(
    node: &Node,
    in_directive_body: bool,
    source: &str,
    edits: &mut Vec<PairEdit>,
) {
    // Directive bodies — mark the immediate body Dict as
    // declaration-shaped so it doesn't get reordered.
    for dir in &node.directives {
        let body = match &dir.body {
            relon_parser::DirectiveBody::Value(b) => Some(b),
            relon_parser::DirectiveBody::NameBody { body, .. } => Some(body),
            _ => None,
        };
        if let Some(b) = body {
            collect_dict_reorder_edits(b, true, source, edits);
        }
    }
    // Decorator args + the expression children below are regular
    // values; reorder applies inside them.
    for dec in &node.decorators {
        for arg in &dec.args {
            collect_dict_reorder_edits(&arg.value, false, source, edits);
        }
    }
    for child in expr_children(node) {
        collect_dict_reorder_edits(child, false, source, edits);
    }
    if in_directive_body {
        return;
    }
    let Expr::Dict(pairs) = &*node.expr else {
        return;
    };
    if pairs.len() < 2 {
        return;
    }
    let classified: Vec<(PairKind, &(TokenKey, Node))> = pairs
        .iter()
        .map(|p| (classify_dict_pair(p), p))
        .collect();
    if methods_already_first(&classified) {
        return;
    }
    // Conservative v1 guard: skip reorder if the body contains a
    // comment. Comment placement is brittle to byte-level reorder
    // because comments live in the token stream, not the AST — we
    // can't know which pair they "belong" to. Files with comments
    // keep their original pair order.
    let (body_start, body_end) = match dict_body_range(node, source) {
        Some(range) => range,
        None => return,
    };
    let body = &source[body_start..body_end];
    if body.contains("//") || body.contains("/*") {
        return;
    }
    // Build the new body text: methods (in original order) then
    // fields (in original order), joined by `,\n`. Outer formatter
    // re-indents the result, so this only has to produce a valid
    // pair sequence.
    let methods = classified.iter().filter(|(c, _)| *c == PairKind::Method);
    let fields = classified.iter().filter(|(c, _)| *c == PairKind::Field);
    let pieces: Vec<&str> = methods
        .chain(fields)
        .map(|(_, pair)| pair_source_slice(source, pair))
        .collect();
    let new_body = format!("\n{}\n", pieces.join(",\n"));
    edits.push(PairEdit {
        start: body_start,
        end: body_end,
        replacement: new_body,
    });
}

fn classify_dict_pair(pair: &(TokenKey, Node)) -> PairKind {
    let (_, value) = pair;
    match &*value.expr {
        Expr::Closure { .. } => PairKind::Method,
        _ => PairKind::Field,
    }
}

fn methods_already_first(classified: &[(PairKind, &(TokenKey, Node))]) -> bool {
    let mut seen_field = false;
    for (kind, _) in classified {
        match kind {
            PairKind::Field => seen_field = true,
            PairKind::Method if seen_field => return false,
            PairKind::Method => {}
        }
    }
    true
}

/// Locate the byte range *inside* a Dict's braces (exclusive of
/// `{` and `}`). Scans the source from the Dict node's range start
/// for the first `{`, then walks forward with depth tracking for
/// the matching `}`. Returns `None` if the braces can't be found —
/// defensive; should not happen for a parsed Dict.
fn dict_body_range(node: &Node, source: &str) -> Option<(usize, usize)> {
    let span_start = node.range.start.offset;
    let span_end = node.range.end.offset.min(source.len());
    let span = &source[span_start..span_end];
    let open_rel = span.find('{')?;
    let open_abs = span_start + open_rel;
    let mut depth = 0i32;
    let bytes = source.as_bytes();
    let mut i = open_abs;
    while i < span_end {
        let c = bytes[i] as char;
        match c {
            '"' => {
                // Skip over a string literal (with backslash escape
                // awareness) so an inner `{` / `}` doesn't move the
                // depth counter.
                i += 1;
                while i < span_end {
                    let cc = bytes[i] as char;
                    if cc == '\\' {
                        i += 2;
                        continue;
                    }
                    if cc == '"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open_abs + 1, i));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Source slice covering an entire Dict pair, including any
/// directives / decorators attached to its value node (which
/// precede the key in the source). Used to copy pair text verbatim
/// when reordering.
fn pair_source_slice<'a>(source: &'a str, pair: &(TokenKey, Node)) -> &'a str {
    let (key, value) = pair;
    let mut start = key_start_offset(key).unwrap_or(value.range.start.offset);
    for dir in &value.directives {
        start = start.min(dir.range.start.offset);
    }
    for dec in &value.decorators {
        start = start.min(dec.range.start.offset);
    }
    let end = value.range.end.offset.min(source.len());
    source[start..end].trim().into()
}

fn key_start_offset(key: &TokenKey) -> Option<usize> {
    match key {
        TokenKey::String(_, range, _) => Some(range.start.offset),
        TokenKey::Dynamic(node, _) => Some(node.range.start.offset),
        TokenKey::Spread(range) => Some(range.start.offset),
        _ => None,
    }
}

/// Yield expression children only — decorators and directives are
/// walked separately by the reorder driver so it can flag the
/// immediate body of a `#schema` / `#extend` / similar declaration
/// directive as "don't reorder me".
fn expr_children(node: &Node) -> Vec<&Node> {
    let mut out = Vec::new();
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
            if let Some(c) = condition {
                out.push(c);
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
                if let relon_parser::FStringPart::Interpolation(n) = part {
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
        _ => {}
    }
    out
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
    fn methods_lifted_to_front_of_dict() {
        // Scrambled order: field, method, field, method. After fmt,
        // methods come first (preserving relative order), then fields
        // (also in original order), with a blank line between the
        // two groups.
        let source = "{\n    project: { name: \"x\" },\n    multiply(a, b): a * b,\n    meta: { count: 3 },\n    currency(v, s): v + \" \" + s\n}\n";
        let formatted = format_source(source).unwrap();
        let methods_idx = formatted.find("multiply").unwrap();
        let currency_idx = formatted.find("currency").unwrap();
        let project_idx = formatted.find("project:").unwrap();
        let meta_idx = formatted.find("meta:").unwrap();
        assert!(methods_idx < currency_idx, "method order preserved");
        assert!(currency_idx < project_idx, "methods land before fields");
        assert!(project_idx < meta_idx, "field order preserved");
        // Idempotent: second pass produces the same output.
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn schema_body_field_order_preserved() {
        // Schema fields with predicate-shaped closure values must
        // not get reordered — schemas are declarations, not Dict
        // bodies in the reorder-policy sense.
        let source = "#schema User {\n    String name: *,\n    Int age: (a) => a >= 0\n}\n\n{\n    x: 1\n}\n";
        let formatted = format_source(source).unwrap();
        let name_idx = formatted.find("String name").unwrap();
        let age_idx = formatted.find("Int age").unwrap();
        assert!(name_idx < age_idx, "schema field order preserved: {formatted}");
    }

    #[test]
    fn comments_disable_reorder() {
        // Conservative v1: any comment inside the Dict body disables
        // reorder for that Dict (comments can't be statically routed
        // to a specific pair).
        let source = "{\n    // keep me\n    project: { x: 1 },\n    multiply(a, b): a * b\n}\n";
        let formatted = format_source(source).unwrap();
        let project_idx = formatted.find("project:").unwrap();
        let multiply_idx = formatted.find("multiply").unwrap();
        assert!(
            project_idx < multiply_idx,
            "expected original order kept when comments present: {formatted}"
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
