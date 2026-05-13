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
    // Parse for syntactic validation; the formatter itself runs over
    // the token stream so it preserves comment placement byte-for-byte.
    parse_document(source).map_err(|error| Error::Parse(error.to_string()))?;
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
    /// `#import { a, b } from "..."` destructure list — kept inline:
    /// `{ a, b }` with single spaces inside and `, ` between entries.
    /// Not a Dict; no indent change, no newlines inside.
    ImportDestructure,
}

/// Tracks whether we're between `#import` and the path string at the
/// end of the directive. Set when we emit a `#` whose next word is
/// `import`; cleared after we emit the path string. Drives:
///   - `{` after the import keyword becomes an inline destructure.
///   - `*` after the import keyword is the spread wildcard (not
///     multiplication).
///   - Blank-line layout never wedges between `#import` and its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportPhase {
    /// Not inside an import directive.
    None,
    /// Between `#` and the path string (anywhere in the body).
    Inside,
    /// Just emitted the path string — next `#import` should pack
    /// against this one (no blank separator). Cleared on the next
    /// non-import top-level construct.
    JustFinished,
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
    import_phase: ImportPhase,
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
            import_phase: ImportPhase::None,
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
                        if self.import_phase == ImportPhase::Inside {
                            // `#import { a, b } from "..."` — destructure
                            // list kept on one line.
                            self.write_value_prefix();
                            self.write_plain("{");
                            self.space();
                            self.frames.push(Frame::ImportDestructure);
                            return Some(TokenKind::OpenBrace);
                        }
                        Some(self.format_open_multiline(token, TokenKind::CloseBrace, Frame::Brace))
                    }
                    TokenKind::CloseBrace if self.top_frame() == Some(Frame::ImportDestructure) => {
                        self.space();
                        self.write_plain("}");
                        self.frames.pop();
                        Some(TokenKind::CloseBrace)
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
                        self.handle_top_level_hash();
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
                        self.write_atom(token.text);
                        // The path string is the last token of an
                        // `#import` directive — mark "just finished"
                        // so the next `#import` can pack against it
                        // without a blank separator.
                        if token.kind == TokenKind::String
                            && self.import_phase == ImportPhase::Inside
                            && self.frames.is_empty()
                        {
                            self.import_phase = ImportPhase::JustFinished;
                        }
                        Some(token.kind)
                    }
                    TokenKind::LineComment | TokenKind::BlockComment => unreachable!(),
                }
            }
        }
    }

    /// Apply blank-line rules for top-level `#` directives. Called
    /// just before emitting the `#`. Looks ahead at the next word to
    /// decide whether to insert a blank separator.
    ///
    /// Rules:
    ///   - `#schema` / `#extend` / `#main` always get a blank above
    ///     them when not the first thing in the file.
    ///   - `#import` gets a blank above unless the previous content
    ///     was already an `#import` (consecutive imports pack tight).
    ///   - Non-block directives (pair-level pragmas like `#private`,
    ///     `#expect`, `#derive`) get no special treatment — they're
    ///     attached to their following pair.
    fn handle_top_level_hash(&mut self) {
        if !self.frames.is_empty() {
            return;
        }
        let Some(next) = self.tokens.get(self.index + 1) else {
            return;
        };
        if next.kind != TokenKind::Word {
            return;
        }
        match next.text {
            "import" => {
                if self.import_phase == ImportPhase::JustFinished {
                    if !self.line_start {
                        self.newline();
                    }
                } else {
                    self.ensure_blank_line_separator();
                }
                self.import_phase = ImportPhase::Inside;
            }
            "schema" | "extend" | "main" => {
                self.ensure_blank_line_separator();
                self.import_phase = ImportPhase::None;
            }
            _ => {}
        }
    }

    fn format_open_multiline(
        &mut self,
        token: Token<'a>,
        close_kind: TokenKind,
        frame: Frame,
    ) -> TokenKind {
        // Blank line before a root-level `{` / `[` whenever the
        // preceding root-level construct was a directive body or an
        // `#import` block — the file's value body must read as its
        // own paragraph. Triggers on either `}` (e.g. after a
        // `#schema X { ... }` body) or after a `JustFinished` import
        // directive.
        if self.frames.is_empty()
            && (self.previous.map(|p| p.kind) == Some(TokenKind::CloseBrace)
                || self.import_phase == ImportPhase::JustFinished)
        {
            self.ensure_blank_line_separator();
            self.import_phase = ImportPhase::None;
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
        self.indent += 1;
        self.newline();
        token.kind
    }

    fn format_close_multiline(&mut self, text: &str, frame: Frame) {
        self.pop_frame(frame);
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
            || matches!(
                self.top_frame(),
                Some(Frame::Paren) | Some(Frame::Index) | Some(Frame::ImportDestructure)
            )
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
        // `*` in value position (no preceding operand) OR right after
        // the `import` keyword (`#import * from ...`) is a wildcard,
        // not multiplication — emit as a bare value so it doesn't pick
        // up binary-operator padding on either side.
        let after_import_keyword = self
            .previous
            .is_some_and(|p| p.kind == TokenKind::Word && p.text == "import");
        let wildcard = text == "*" && (!self.previous_allows_binary() || after_import_keyword);
        if unary || wildcard {
            self.write_value_prefix();
            self.write_plain(text);
            if wildcard {
                self.space_if_next_starts_value();
            }
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

        if matches!(
            self.top_frame(),
            Some(Frame::Paren) | Some(Frame::Index) | Some(Frame::ImportDestructure)
        ) {
            return;
        }

        // `#import …` directive stays on one line — suppress any
        // leading newlines for tokens between the `#` and the path
        // string (inclusive). Without this, a user's incoming
        // `#import\n*\nfrom\n"…"` would format to four separate lines.
        if self.import_phase == ImportPhase::Inside {
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

    /// Emit a blank line before the next token if the output has
    /// already produced non-trivial content. Idempotent: subsequent
    /// calls collapse into a single blank line.
    fn ensure_blank_line_separator(&mut self) {
        if self.output.is_empty() {
            return;
        }
        if !self.line_start {
            self.newline();
        }
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

    /// Inlined preset sources mirroring the playground's default
    /// examples. Each one must round-trip through `format_source` and
    /// be idempotent (`fmt(fmt(x)) == fmt(x)`). The sources here are
    /// the *canonical* expected output of the formatter — if the
    /// preset content in the playground changes, mirror it here.
    mod presets {
        pub const DEMO: &str = "// Try editing me - evaluate runs automatically.\n{\n    currency(val, symbol): val + \" \" + symbol,\n    multiply(a, b): a * b,\n    project: {\n        name: \"Relon Playground\",\n        details: {\n            base_price: 1500,\n            total: multiply(&sibling.base_price, 1.2),\n            @currency(\"GBP\")\n            display: &sibling.total\n        }\n    },\n    meta: {\n        tags_count: len([\"rust\", \"config\", \"dsl\"]),\n        summary: f\"Active project: ${&root.project.name}\"\n    }\n}\n";

        pub const PRICING: &str = "/*\n  Invoice pricing with tiered discounts and tax.\n  See examples/pricing.relon in the repo for the full annotated source.\n*/\n#schema LineItem {\n    String sku: *,\n    #expect \"qty must be > 0\"\n    Int qty: (n) => n > 0,\n    #expect \"unit_price must be >= 0\"\n    Float unit_price: (p) => p >= 0\n}\n\n#schema Order {\n    List<LineItem> items: *,\n    #expect \"tier must be one of: standard / gold\"\n    String tier: (t) => t == \"standard\" || t == \"gold\"\n}\n\n#main(Order order)\n{\n    #private\n    currency(symbol, val): symbol + \" \" + val,\n    #private\n    volume_rate(sub): sub >= 1000 ? 0.10: sub >= 500 ? 0.05: 0.0,\n    #private\n    loyalty_rate(tier): tier == \"gold\" ? 0.03: 0.0,\n    #private\n    tax_rate: 0.08,\n    #private\n    sum_floats(xs): _list_reduce(xs, 0.0, (a, x) => a + x),\n    subtotal: sum_floats([it.qty * it.unit_price for it in order.items]),\n    discount_rate: volume_rate(&sibling.subtotal) + loyalty_rate(order.tier),\n    discount_amount: &sibling.subtotal * &sibling.discount_rate,\n    taxable: &sibling.subtotal - &sibling.discount_amount,\n    tax_amount: &sibling.taxable * tax_rate,\n    total: &sibling.taxable + &sibling.tax_amount,\n    @currency(\"USD\")\n    total_display: &sibling.total\n}\n";

        pub const FEATURE_FLAG: &str = "/*\n  Runtime feature-flag evaluator.\n\n  Percentage rollouts need a host-registered `native_hash(s) -> Int`.\n  See examples/feature_flag.relon for the full annotated source.\n*/\n#schema User {\n    String id: *,\n    String region: (r) => r == \"us\" || r == \"eu\" || r == \"apac\",\n    String plan: (p) => p == \"free\" || p == \"pro\" || p == \"enterprise\"\n}\n\n#main(User user) -> Dict<String, Dict<String, Bool>>\n{\n    #private\n    hash_mod_100(s): native_hash(s) % 100,\n    #private\n    rules: {\n        legacy_checkout: (u) => false,\n        dark_mode: (u) => true,\n        gdpr_banner: (u) => u.region == \"eu\",\n        advanced_editor: (u) => u.plan == \"pro\" || u.plan == \"enterprise\",\n        new_search: (u) => hash_mod_100(u.id) < 25\n    },\n    flags: {\n        legacy_checkout: rules.legacy_checkout(user),\n        dark_mode: rules.dark_mode(user),\n        gdpr_banner: rules.gdpr_banner(user),\n        advanced_editor: rules.advanced_editor(user),\n        new_search: rules.new_search(user)\n    }\n}\n";

        pub const WORKFLOW: &str = "/*\n  Order workflow as a data-driven state machine.\n\n  Try via the CLI:\n    cargo run -q -p relon-cli -- run examples/workflow.relon \\\n        --args '{\"state\": \"placed\", \"event\": \"pay\"}'\n*/\n#schema Transition {\n    String from: (s) => s == \"placed\" || s == \"paid\" || s == \"shipped\",\n    String on: *,\n    String to: (s) => s == \"paid\" || s == \"shipped\" || s == \"delivered\" || s == \"cancelled\",\n    List<String> emit: *\n}\n\n#main(String state, String event)\n{\n    #private\n    transitions: [\n        #brand Transition {\n            from: \"placed\",\n            on: \"pay\",\n            to: \"paid\",\n            emit: [\n                \"charge_card\",\n                \"log_payment\"\n            ]\n        },\n        #brand Transition {\n            from: \"paid\",\n            on: \"ship\",\n            to: \"shipped\",\n            emit: [\n                \"notify_shipper\",\n                \"email_user\"\n            ]\n        },\n        #brand Transition {\n            from: \"shipped\",\n            on: \"deliver\",\n            to: \"delivered\",\n            emit: [\n                \"email_user\"\n            ]\n        },\n        #brand Transition {\n            from: \"placed\",\n            on: \"cancel\",\n            to: \"cancelled\",\n            emit: []\n        },\n        #brand Transition {\n            from: \"paid\",\n            on: \"cancel\",\n            to: \"cancelled\",\n            emit: [\n                \"refund_card\"\n            ]\n        }\n    ],\n    #private\n    match_one(t): t.from == state && t.on == event,\n    #private\n    matched: _list_filter(&sibling.transitions, &sibling.match_one),\n    next_state: len(matched) > 0 ? matched[0].to: state,\n    emit: len(matched) > 0 ? matched[0].emit: [\"unhandled_event\"]\n}\n";

        pub const MODULES: &str = "// Three #import shapes — try Mod-clicking any imported name to\n// jump across files.\n#import lib from \"./lib.relon\"\n#import { format_price } from \"./lib.relon\"\n#import * from \"./lib.relon\"\n\n{\n    namespaced: lib.with_tax(100.0, 0.08),\n    destructured: format_price(199.99, \"USD\"),\n    spread: discount(50.0, 0.15)\n}\n";

        pub const MODULES_LIB: &str = "// Pricing helpers shared by main.relon.\n{\n    with_tax(amount, rate): amount * (1.0 + rate),\n    format_price(value, symbol): symbol + \" \" + value,\n    discount(amount, rate): amount * (1.0 - rate)\n}\n";
    }

    /// Helper: assert that formatting the preset source succeeds, is
    /// idempotent, and parses back. Does NOT require the source to be
    /// pre-canonical — the formatter's output of the playground
    /// source is allowed to differ stylistically from the source
    /// (e.g. lists expanded). The key invariant is that running
    /// Format twice produces the same result as running it once.
    fn assert_preset(source: &str) {
        let once = format_source(source)
            .unwrap_or_else(|e| panic!("format failed: {e}\n--- source ---\n{source}"));
        let twice = format_source(&once)
            .unwrap_or_else(|e| panic!("re-format failed: {e}\n--- once ---\n{once}"));
        assert_eq!(
            once, twice,
            "fmt is not idempotent.\n--- once ---\n{once}\n--- twice ---\n{twice}"
        );
    }

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
        let source = "{\n    abs(x): x < 0 ? -x: x\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn comparison_lt_gt_unchanged() {
        let source = "{\n    cmp(a, b): a < b ? a: b\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn arrow_token_keeps_compact() {
        let source = "#main(Int x) -> Int\n{\n    n: x\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_with_block_round_trip() {
        let source = "#schema Money {\n    Int cents: *\n} with {\n    cents_value() -> Int: self.cents\n}\n\n{\n    Money price: {\n        cents: 100\n    }\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn formats_with_block_derive_pragma() {
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
        let source = "#schema User {\n    String name: *,\n    Int age: (a) => a >= 0\n}\n\n{\n    x: 1\n}\n";
        let formatted = format_source(source).unwrap();
        assert!(formatted.contains("String name: *,"), "expected `*,` flush: {formatted}");
        assert!(!formatted.contains("* ,"));
    }

    #[test]
    fn block_directives_get_blank_separator() {
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
    fn import_destructure_inline() {
        // `#import { a, b } from "..."` keeps the destructure on one
        // line even when input had it split across newlines.
        let split = "#import {\n    format_price\n}\nfrom \"./lib.relon\"\n{\n    x: 1\n}\n";
        let formatted = format_source(split).unwrap();
        assert!(
            formatted.contains("#import { format_price } from \"./lib.relon\""),
            "destructure should be inline: {formatted}"
        );
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn import_spread_star_no_binary_padding() {
        // `#import * from "..."` — `*` is a wildcard, not multiplication.
        let source = "#import * from \"./lib.relon\"\n\n{\n    x: 1\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn consecutive_imports_pack_tight() {
        // Multiple #import directives sit on consecutive lines (no
        // blank between them); a blank line separates the import
        // block from the file body.
        let source = "#import a from \"./a.relon\"\n#import b from \"./b.relon\"\n#import * from \"./c.relon\"\n\n{\n    x: 1\n}\n";
        let formatted = format_source(source).unwrap();
        assert_eq!(formatted, source);
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }

    #[test]
    fn preset_demo_idempotent() {
        assert_preset(presets::DEMO);
    }

    #[test]
    fn preset_pricing_idempotent() {
        assert_preset(presets::PRICING);
    }

    #[test]
    fn preset_feature_flag_idempotent() {
        assert_preset(presets::FEATURE_FLAG);
    }

    #[test]
    fn preset_workflow_idempotent() {
        assert_preset(presets::WORKFLOW);
    }

    #[test]
    fn preset_modules_idempotent() {
        assert_preset(presets::MODULES);
    }

    #[test]
    fn preset_modules_lib_idempotent() {
        assert_preset(presets::MODULES_LIB);
    }

    // ----- structural regression tests -----------------------------
    // These guard against the catastrophic and visible bugs the user
    // hit on 2026-05-13: directives getting orphaned from their
    // pairs, and Dict bodies getting written into the wrong braces.

    #[test]
    fn pricing_schema_bodies_unchanged_by_format() {
        // The `#schema LineItem` body owns `String sku / Int qty /
        // Float unit_price`. A previous reorder pre-pass
        // mis-identified the root Dict's brace range as the schema
        // body's range and overwrote it with `#main`'s methods. After
        // format, both schema bodies must still contain their
        // original declarations.
        let formatted = format_source(presets::PRICING).unwrap();
        assert!(
            formatted.contains("String sku:") && formatted.contains("Int qty:")
                && formatted.contains("Float unit_price:"),
            "#schema LineItem body lost its declarations after format:\n{formatted}"
        );
        assert!(
            formatted.contains("List<LineItem> items:") && formatted.contains("String tier:"),
            "#schema Order body lost its declarations after format:\n{formatted}"
        );
        // None of #main's methods should leak into either schema.
        let schema_section = &formatted[..formatted
            .find("#main(")
            .expect("expected #main block in pricing preset")];
        assert!(
            !schema_section.contains("currency(symbol, val)"),
            "method `currency` leaked into schema section:\n{schema_section}"
        );
        assert!(
            !schema_section.contains("volume_rate("),
            "method `volume_rate` leaked into schema section:\n{schema_section}"
        );
    }

    #[test]
    fn feature_flag_private_attached_to_key() {
        // `#private` is a pair-level pragma — it MUST sit on the
        // immediately-preceding line of its pair's key. A previous
        // paragraph-break pre-pass inserted a blank between
        // `#private` and `rules:`, severing the ownership.
        let formatted = format_source(presets::FEATURE_FLAG).unwrap();
        assert!(
            formatted.contains("#private\n    rules:"),
            "#private must sit directly above `rules:` with no blank line:\n{formatted}"
        );
        assert!(
            formatted.contains("#private\n    hash_mod_100("),
            "#private must sit directly above `hash_mod_100(`:\n{formatted}"
        );
        // Defensive: no double-newline between `#private` and any pair key.
        assert!(
            !formatted.contains("#private\n\n"),
            "found a blank line directly after `#private`:\n{formatted}"
        );
    }

    #[test]
    fn pricing_private_attached_to_key() {
        let formatted = format_source(presets::PRICING).unwrap();
        for pair in [
            "currency(symbol, val):",
            "volume_rate(sub):",
            "loyalty_rate(tier):",
            "tax_rate:",
            "sum_floats(xs):",
        ] {
            let expected = format!("#private\n    {pair}");
            assert!(
                formatted.contains(&expected),
                "#private must sit directly above `{pair}`:\n{formatted}"
            );
        }
        assert!(
            !formatted.contains("#private\n\n"),
            "found a blank line directly after `#private`:\n{formatted}"
        );
    }

    #[test]
    fn pricing_decorator_attached_to_key() {
        // `@currency("USD")` is a decorator attached to
        // `total_display:`. Must stay glued to its key.
        let formatted = format_source(presets::PRICING).unwrap();
        assert!(
            formatted.contains("@currency(\"USD\")\n    total_display:"),
            "@currency decorator must sit directly above `total_display:`:\n{formatted}"
        );
    }

    #[test]
    fn pricing_expect_attached_to_schema_field() {
        // `#expect "msg"` is a pair-level pragma on schema fields.
        // Must stay glued to its field's `Type name:` line.
        let formatted = format_source(presets::PRICING).unwrap();
        assert!(
            formatted.contains("#expect \"qty must be > 0\"\n    Int qty:"),
            "#expect must sit directly above `Int qty:`:\n{formatted}"
        );
        assert!(
            formatted.contains("#expect \"unit_price must be >= 0\"\n    Float unit_price:"),
            "#expect must sit directly above `Float unit_price:`:\n{formatted}"
        );
        assert!(
            formatted.contains("#expect \"tier must be one of: standard / gold\"\n    String tier:"),
            "#expect must sit directly above `String tier:`:\n{formatted}"
        );
    }

    #[test]
    fn workflow_brand_directives_attached_to_dict() {
        // `#brand Transition { ... }` is a Value-shape directive
        // attached to the inline Dict that follows. Must stay
        // adjacent — no blank line between `#brand Transition` and
        // its `{`.
        let formatted = format_source(presets::WORKFLOW).unwrap();
        assert!(
            formatted.contains("#brand Transition {"),
            "#brand Transition must precede its {{ on the same logical block:\n{formatted}"
        );
    }

    #[test]
    fn modules_imports_pack_with_trailing_blank() {
        // Three consecutive #import directives, then the file body.
        // No blank between any pair of #imports; one blank before the
        // root `{`.
        let formatted = format_source(presets::MODULES).unwrap();
        assert!(
            formatted.contains("#import lib from \"./lib.relon\"\n#import { format_price }"),
            "consecutive #imports must pack (no blank between):\n{formatted}"
        );
        assert!(
            formatted.contains("#import * from \"./lib.relon\"\n\n{"),
            "blank line missing between last #import and file body:\n{formatted}"
        );
    }
}
