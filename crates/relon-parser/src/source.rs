#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizeError {
    message: String,
}

impl TokenizeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for TokenizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TokenizeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceToken<'a> {
    pub kind: SourceTokenKind,
    pub text: &'a str,
    pub leading_newlines: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTokenKind {
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
    /// `#` directive sigil (introduced in batch 3).
    Hash,
    Amp,
    Question,
    Ellipsis,
    Operator,
    Equal,
}

pub fn tokenize_source(source: &str) -> Result<Vec<SourceToken<'_>>, TokenizeError> {
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
        tokens.push(SourceToken {
            kind,
            text: &source[start..end],
            leading_newlines,
            start,
            end,
        });
        index = end;
    }

    Ok(tokens)
}

fn scan_token(source: &str, index: usize) -> Result<(SourceTokenKind, usize), TokenizeError> {
    if starts_with_at(source, index, "//") {
        return Ok((
            SourceTokenKind::LineComment,
            scan_line_comment(source, index),
        ));
    }
    if starts_with_at(source, index, "/*") {
        return Ok((
            SourceTokenKind::BlockComment,
            scan_block_comment(source, index)?,
        ));
    }
    if let Some(end) = scan_string_like(source, index)? {
        return Ok((SourceTokenKind::String, end));
    }
    if starts_with_at(source, index, "...") {
        return Ok((SourceTokenKind::Ellipsis, index + 3));
    }
    for op in ["==", "!=", "<=", ">=", "&&", "||", "++", "=>", "->"] {
        if starts_with_at(source, index, op) {
            return Ok((SourceTokenKind::Operator, index + op.len()));
        }
    }

    let (ch, len) = next_char(source, index)
        .ok_or_else(|| TokenizeError::new(format!("unexpected end of input at byte {index}")))?;

    if is_ident_start(ch) {
        return Ok((SourceTokenKind::Word, scan_identifier(source, index)));
    }
    if ch.is_ascii_digit() {
        return Ok((SourceTokenKind::Number, scan_number(source, index)));
    }

    let kind = match ch {
        '{' => SourceTokenKind::OpenBrace,
        '}' => SourceTokenKind::CloseBrace,
        '[' => SourceTokenKind::OpenBracket,
        ']' => SourceTokenKind::CloseBracket,
        '(' => SourceTokenKind::OpenParen,
        ')' => SourceTokenKind::CloseParen,
        ',' => SourceTokenKind::Comma,
        ':' => SourceTokenKind::Colon,
        '.' => SourceTokenKind::Dot,
        '@' => SourceTokenKind::At,
        '#' => SourceTokenKind::Hash,
        '&' => SourceTokenKind::Amp,
        '?' => SourceTokenKind::Question,
        '=' => SourceTokenKind::Equal,
        '+' | '-' | '*' | '/' | '%' | '<' | '>' | '!' | '|' => SourceTokenKind::Operator,
        _ => {
            return Err(TokenizeError::new(format!(
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

fn scan_block_comment(source: &str, start: usize) -> Result<usize, TokenizeError> {
    let body_start = start + 2;
    let Some(relative_end) = source[body_start..].find("*/") else {
        return Err(TokenizeError::new(format!(
            "unterminated block comment at byte {start}"
        )));
    };
    Ok(body_start + relative_end + 2)
}

fn scan_string_like(source: &str, start: usize) -> Result<Option<usize>, TokenizeError> {
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

fn scan_normal_string(source: &str, start: usize) -> Result<usize, TokenizeError> {
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
    Err(TokenizeError::new(format!(
        "unterminated string at byte {start}"
    )))
}

fn scan_raw_string(
    source: &str,
    start: usize,
    prefix_len: usize,
) -> Option<Result<usize, TokenizeError>> {
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
        return Some(Err(TokenizeError::new(format!(
            "unterminated raw string at byte {start}"
        ))));
    };

    Some(Ok(body_start + relative_end + closing.len()))
}

fn scan_f_string(source: &str, start: usize) -> Option<Result<usize, TokenizeError>> {
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

    Some(Err(TokenizeError::new(format!(
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
    fn preserves_comment_tokens_and_leading_newlines() {
        let tokens = tokenize_source("{\n// keep\nfoo: 1 /* block */\n}").unwrap();
        assert_eq!(tokens[0].kind, SourceTokenKind::OpenBrace);
        assert_eq!(tokens[1].kind, SourceTokenKind::LineComment);
        assert_eq!(tokens[1].text, "// keep");
        assert_eq!(tokens[1].leading_newlines, 1);
        assert_eq!(tokens[5].kind, SourceTokenKind::BlockComment);
        assert_eq!(tokens[5].text, "/* block */");
    }

    #[test]
    fn treats_strings_and_f_strings_as_single_source_tokens() {
        let source = r###"{value:f"hello ${ call("x", /* not trivia */ 1) }", raw:r#"// nope"#}"###;
        let tokens = tokenize_source(source).unwrap();
        assert!(tokens.iter().any(|token| {
            token.kind == SourceTokenKind::String && token.text.contains(r#"/* not trivia */"#)
        }));
        assert!(tokens.iter().any(|token| {
            token.kind == SourceTokenKind::String && token.text == r##"r#"// nope"#"##
        }));
        assert!(!tokens
            .iter()
            .any(|token| token.kind == SourceTokenKind::BlockComment));
        assert!(!tokens
            .iter()
            .any(|token| token.kind == SourceTokenKind::LineComment));
    }
}
