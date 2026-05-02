use relon_parser::{parse_base, soc0, Span};
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
    let tokens = tokenize(source)?;
    let mut formatter = SourceFormatter::new(&tokens);
    let output = formatter.format();
    validate_source(&output)?;
    Ok(output)
}

pub fn is_formatted(source: &str) -> Result<bool, Error> {
    Ok(format_source(source)? == source)
}

fn validate_source(source: &str) -> Result<(), Error> {
    let mut input = Span::new(source);
    parse_base(&mut input).map_err(|error| Error::Parse(format!("{error:?}")))?;
    soc0(&mut input).map_err(|error| Error::Parse(format!("{error:?}")))?;
    if input.is_empty() {
        Ok(())
    } else {
        Err(Error::Parse("trailing input after root value".to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Token<'a> {
    kind: TokenKind,
    text: &'a str,
    leading_newlines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Number,
    String,
    LineComment,
    BlockComment,
    OpenBrace,
    CloseBrace,
    OpenBracket,
    CloseBracket,
    OpenParen,
    CloseParen,
    Comma,
    Colon,
    Dot,
    At,
    Amp,
    Question,
    Ellipsis,
    Operator,
    Equal,
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
                    TokenKind::Amp => {
                        self.write_value_prefix();
                        self.write_plain("&");
                        Some(TokenKind::Amp)
                    }
                    TokenKind::Question => {
                        self.write_binary_operator("?");
                        Some(TokenKind::Question)
                    }
                    TokenKind::Ellipsis => {
                        self.write_value_prefix();
                        self.write_plain("...");
                        Some(TokenKind::Ellipsis)
                    }
                    TokenKind::Operator => {
                        self.format_operator(token.text);
                        Some(TokenKind::Operator)
                    }
                    TokenKind::Equal => {
                        self.write_plain("=");
                        Some(TokenKind::Equal)
                    }
                    TokenKind::Word | TokenKind::Number | TokenKind::String => {
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
        if self.next_is_inline_line_comment() {
            self.space();
        } else if self.top_frame() == Some(Frame::Paren) || self.top_frame() == Some(Frame::Index) {
            self.space();
        } else {
            self.newline();
        }
    }

    fn format_operator(&mut self, text: &str) {
        let unary = text == "!" || ((text == "-" || text == "+") && !self.previous_allows_binary());
        if unary {
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
        if token.leading_newlines == 0 || self.line_start {
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

fn tokenize(source: &str) -> Result<Vec<Token<'_>>, Error> {
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < source.len() {
        let mut leading_newlines = 0;
        while let Some((ch, len)) = next_char(source, index) {
            if !ch.is_whitespace() {
                break;
            }
            if ch == '\n' || ch == '\r' {
                leading_newlines += 1;
            }
            index += len;
        }

        if index >= source.len() {
            break;
        }

        let start = index;
        let (kind, end) = scan_token(source, index)?;
        tokens.push(Token {
            kind,
            text: &source[start..end],
            leading_newlines,
        });
        index = end;
    }

    Ok(tokens)
}

fn scan_token(source: &str, index: usize) -> Result<(TokenKind, usize), Error> {
    if starts_with_at(source, index, "//") {
        return Ok((TokenKind::LineComment, scan_line_comment(source, index)));
    }
    if starts_with_at(source, index, "/*") {
        return Ok((TokenKind::BlockComment, scan_block_comment(source, index)?));
    }
    if let Some(end) = scan_string_like(source, index)? {
        return Ok((TokenKind::String, end));
    }
    if starts_with_at(source, index, "...") {
        return Ok((TokenKind::Ellipsis, index + 3));
    }
    for op in ["==", "!=", "<=", ">=", "&&", "||", "++"] {
        if starts_with_at(source, index, op) {
            return Ok((TokenKind::Operator, index + op.len()));
        }
    }

    let (ch, len) = next_char(source, index)
        .ok_or_else(|| Error::Tokenize(format!("unexpected end of input at byte {index}")))?;

    if is_ident_start(ch) {
        return Ok((TokenKind::Word, scan_identifier(source, index)));
    }
    if ch.is_ascii_digit() {
        return Ok((TokenKind::Number, scan_number(source, index)));
    }

    let kind = match ch {
        '{' => TokenKind::OpenBrace,
        '}' => TokenKind::CloseBrace,
        '[' => TokenKind::OpenBracket,
        ']' => TokenKind::CloseBracket,
        '(' => TokenKind::OpenParen,
        ')' => TokenKind::CloseParen,
        ',' => TokenKind::Comma,
        ':' => TokenKind::Colon,
        '.' => TokenKind::Dot,
        '@' => TokenKind::At,
        '&' => TokenKind::Amp,
        '?' => TokenKind::Question,
        '=' => TokenKind::Equal,
        '+' | '-' | '*' | '/' | '%' | '<' | '>' | '!' | '|' => TokenKind::Operator,
        _ => {
            return Err(Error::Tokenize(format!(
                "unexpected character {ch:?} at byte {index}"
            )))
        }
    };

    Ok((kind, index + len))
}

fn scan_identifier(source: &str, start: usize) -> usize {
    let mut index = start;
    while let Some((ch, len)) = next_char(source, index) {
        if !(ch == '_' || ch.is_ascii_alphanumeric()) {
            break;
        }
        index += len;
    }
    index
}

fn scan_number(source: &str, start: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = start;

    if bytes.get(index) == Some(&b'0') {
        if matches!(bytes.get(index + 1), Some(b'x' | b'X')) {
            index += 2;
            while bytes
                .get(index)
                .is_some_and(|byte| byte.is_ascii_hexdigit())
            {
                index += 1;
            }
            return index;
        }
        if matches!(bytes.get(index + 1), Some(b'o' | b'O')) {
            index += 2;
            while bytes
                .get(index)
                .is_some_and(|byte| matches!(byte, b'0'..=b'7'))
            {
                index += 1;
            }
            return index;
        }
        if matches!(bytes.get(index + 1), Some(b'b' | b'B')) {
            index += 2;
            while bytes
                .get(index)
                .is_some_and(|byte| matches!(byte, b'0' | b'1'))
            {
                index += 1;
            }
            return index;
        }
    }

    while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
        index += 1;
    }

    if bytes.get(index) == Some(&b'.')
        && bytes
            .get(index + 1)
            .is_some_and(|byte| byte.is_ascii_digit())
    {
        index += 1;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }
    }

    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let checkpoint = index;
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }

        let digits_start = index;
        while bytes.get(index).is_some_and(|byte| byte.is_ascii_digit()) {
            index += 1;
        }

        if index == digits_start {
            index = checkpoint;
        }
    }

    index
}

fn scan_line_comment(source: &str, start: usize) -> usize {
    let mut index = start + 2;
    while let Some((ch, len)) = next_char(source, index) {
        if ch == '\n' || ch == '\r' {
            break;
        }
        index += len;
    }
    index
}

fn scan_block_comment(source: &str, start: usize) -> Result<usize, Error> {
    let body_start = start + 2;
    let Some(relative_end) = source[body_start..].find("*/") else {
        return Err(Error::Tokenize(format!(
            "unterminated block comment at byte {start}"
        )));
    };
    Ok(body_start + relative_end + 2)
}

fn scan_string_like(source: &str, start: usize) -> Result<Option<usize>, Error> {
    if starts_with_at(source, start, "\"") {
        return scan_normal_string(source, start).map(Some);
    }
    if starts_with_at(source, start, "r") {
        return scan_raw_string(source, start, 1).transpose();
    }
    if starts_with_at(source, start, "f") {
        return scan_f_string(source, start).transpose();
    }
    Ok(None)
}

fn scan_normal_string(source: &str, start: usize) -> Result<usize, Error> {
    let mut index = start + 1;
    while let Some((ch, len)) = next_char(source, index) {
        index += len;
        if ch == '\\' {
            if let Some((_, next_len)) = next_char(source, index) {
                index += next_len;
            }
            continue;
        }
        if ch == '"' {
            return Ok(index);
        }
    }
    Err(Error::Tokenize(format!(
        "unterminated string at byte {start}"
    )))
}

fn scan_raw_string(source: &str, start: usize, prefix_len: usize) -> Option<Result<usize, Error>> {
    let mut quote = start + prefix_len;
    while starts_with_at(source, quote, "#") {
        quote += 1;
    }

    if !starts_with_at(source, quote, "\"") {
        return None;
    }

    let hashes = quote - start - prefix_len;
    let mut closing = String::from("\"");
    closing.push_str(&"#".repeat(hashes));

    let body_start = quote + 1;
    let Some(relative_end) = source[body_start..].find(&closing) else {
        return Some(Err(Error::Tokenize(format!(
            "unterminated raw string at byte {start}"
        ))));
    };

    Some(Ok(body_start + relative_end + closing.len()))
}

fn scan_f_string(source: &str, start: usize) -> Option<Result<usize, Error>> {
    let mut quote = start + 1;
    while starts_with_at(source, quote, "#") {
        quote += 1;
    }

    if !starts_with_at(source, quote, "\"") {
        return None;
    }

    let hashes = quote - start - 1;
    let mut closing = String::from("\"");
    closing.push_str(&"#".repeat(hashes));

    let mut index = quote + 1;
    let mut interpolation_depth = 0usize;
    while index < source.len() {
        if interpolation_depth == 0 {
            if starts_with_at(source, index, &closing) {
                return Some(Ok(index + closing.len()));
            }
            if starts_with_at(source, index, "${") {
                interpolation_depth = 1;
                index += 2;
                continue;
            }

            let Some((ch, len)) = next_char(source, index) else {
                break;
            };
            index += len;
            if hashes == 0 && ch == '\\' {
                if let Some((_, next_len)) = next_char(source, index) {
                    index += next_len;
                }
            }
            continue;
        }

        if starts_with_at(source, index, "//") {
            index = scan_line_comment(source, index);
            continue;
        }
        if starts_with_at(source, index, "/*") {
            match scan_block_comment(source, index) {
                Ok(end) => {
                    index = end;
                    continue;
                }
                Err(error) => return Some(Err(error)),
            }
        }
        match scan_string_like(source, index) {
            Ok(Some(end)) => {
                index = end;
                continue;
            }
            Ok(None) => {}
            Err(error) => return Some(Err(error)),
        }

        let Some((ch, len)) = next_char(source, index) else {
            break;
        };
        index += len;
        match ch {
            '{' => interpolation_depth += 1,
            '}' => interpolation_depth = interpolation_depth.saturating_sub(1),
            _ => {}
        }
    }

    Some(Err(Error::Tokenize(format!(
        "unterminated f-string at byte {start}"
    ))))
}

fn starts_with_at(source: &str, index: usize, needle: &str) -> bool {
    source
        .get(index..)
        .is_some_and(|remaining| remaining.starts_with(needle))
}

fn next_char(source: &str, index: usize) -> Option<(char, usize)> {
    source
        .get(index..)?
        .chars()
        .next()
        .map(|ch| (ch, ch.len_utf8()))
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
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
}
