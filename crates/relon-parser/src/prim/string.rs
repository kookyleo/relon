use crate::{create_range, Expr, Node, Span};
use winnow::ascii::{hex_digit1, multispace1};
use winnow::combinator::{alt, delimited, preceded, repeat};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};
use winnow::token::{any, literal, take_until, take_while};

/// Parse double-quoted strings and raw strings.
pub fn parse_string<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();

    let s = alt((normal_string, raw_string)).parse_next(input)?;

    let end = input.checkpoint();
    Ok(Node::new(
        Expr::String(s),
        create_range(input.offset_from(&start), input.offset_from(&end)),
    ))
}

fn normal_string<'a>(input: &mut Span<'a>) -> ModalResult<String> {
    delimited('"', string_content, '"').parse_next(input)
}

fn string_content<'a>(input: &mut Span<'a>) -> ModalResult<String> {
    repeat(
        0..,
        alt((
            parse_escaped_char,
            parse_escaped_whitespace.value('\0'), // Special marker to ignore
            any.verify(|c| *c != '"' && *c != '\\'),
        )),
    )
    .map(|v: Vec<char>| v.into_iter().filter(|c| *c != '\0').collect())
    .parse_next(input)
}

pub(crate) fn parse_escaped_char<'a>(input: &mut Span<'a>) -> ModalResult<char> {
    preceded(
        '\\',
        alt((
            parse_unicode,
            'n'.value('\n'),
            'r'.value('\r'),
            't'.value('\t'),
            'b'.value('\u{08}'),
            'f'.value('\u{0C}'),
            '\\'.value('\\'),
            '/'.value('/'),
            '"'.value('"'),
        )),
    )
    .parse_next(input)
}

fn parse_unicode<'a>(input: &mut Span<'a>) -> ModalResult<char> {
    preceded(
        'u',
        alt((
            delimited(
                '{',
                hex_digit1.verify(|s: &str| (1..=6).contains(&s.len())),
                '}',
            ),
            hex_digit1.verify(|s: &str| s.len() == 4),
        )),
    )
    .try_map(|hex| u32::from_str_radix(hex, 16))
    .verify_map(std::char::from_u32)
    .parse_next(input)
}

pub(crate) fn parse_escaped_whitespace<'a>(input: &mut Span<'a>) -> ModalResult<()> {
    preceded('\\', multispace1).void().parse_next(input)
}

fn raw_string<'a>(input: &mut Span<'a>) -> ModalResult<String> {
    preceded('r', hash_string_snippet).parse_next(input)
}

pub(crate) fn hash_string_snippet<'a>(input: &mut Span<'a>) -> ModalResult<String> {
    let hashes: &str = take_while(0.., '#').parse_next(input)?;
    let hash_count = hashes.len();

    literal('"').parse_next(input)?;

    let mut closing = String::from("\"");
    for _ in 0..hash_count {
        closing.push('#');
    }

    let content: &str = take_until(0.., closing.as_str()).parse_next(input)?;
    literal(closing.as_str()).parse_next(input)?;

    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_esc() {
        let mut s = Span::new("\\n");
        assert_eq!(parse_escaped_char(&mut s).unwrap(), '\n');
    }

    #[test]
    fn test_empty() {
        let mut s = Span::new("\"\"");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("".to_string())
        );

        let mut s = Span::new("\"\" ");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("".to_string())
        );

        let mut s = Span::new("r\"\"");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("".to_string())
        );

        let mut s = Span::new("r\"\" ");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("".to_string())
        );

        let mut s = Span::new("r#\"\"#");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("".to_string())
        );
    }

    #[test]
    fn test_parse_escaped_whitespace() {
        let mut s = Span::new("\\   ");
        assert!(parse_escaped_whitespace(&mut s).is_ok());

        let mut s = Span::new("\\\n        ");
        assert!(parse_escaped_whitespace(&mut s).is_ok());
    }

    #[test]
    fn test_parse_escaped_char() {
        let mut s = Span::new("\\n");
        assert_eq!(parse_escaped_char(&mut s).unwrap(), '\n');

        let mut s = Span::new("\\t");
        assert_eq!(parse_escaped_char(&mut s).unwrap(), '\t');

        let mut s = Span::new("\\r");
        assert_eq!(parse_escaped_char(&mut s).unwrap(), '\r');

        let mut s = Span::new("\\u{1F601}");
        assert_eq!(parse_escaped_char(&mut s).unwrap(), '😁');

        let mut s = Span::new("\\uFE0F");
        assert_eq!(parse_escaped_char(&mut s).unwrap(), '\u{FE0F}');

        let mut s = Span::new("\\u{FE0F}");
        assert_eq!(parse_escaped_char(&mut s).unwrap(), '\u{FE0F}');
    }

    #[test]
    fn test_normal_string_content() {
        let mut s = Span::new("hello");
        assert_eq!(string_content(&mut s).unwrap(), "hello".to_string());

        let mut s = Span::new("hello \\n John");
        // string_content does NOT stop at whitespace, it stops at " or \ (if not valid escape)
        // actually in relon-parser it stops at " or \
        // and parse_escaped_char handles \n
        assert_eq!(string_content(&mut s).unwrap(), "hello \n John".to_string());

        let mut s = Span::new("hello \\u{1F601} John");
        assert_eq!(string_content(&mut s).unwrap(), "hello 😁 John".to_string());
    }

    #[test]
    fn test_hash_string() {
        let mut s = Span::new("\"hello \\n John\"");
        assert_eq!(
            hash_string_snippet(&mut s).unwrap(),
            "hello \\n John".to_string()
        );

        let mut s = Span::new("#\"hello ${name}\"#");
        assert_eq!(
            hash_string_snippet(&mut s).unwrap(),
            "hello ${name}".to_string()
        );

        let mut s = Span::new("##\"hello ${name}\"##");
        assert_eq!(
            hash_string_snippet(&mut s).unwrap(),
            "hello ${name}".to_string()
        );
    }

    #[test]
    fn test_parse_string() {
        let mut s = Span::new("\"John\"");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("John".to_string())
        );

        let mut s = Span::new("\"hello \\\n        John\"");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("hello John".to_string())
        );

        let mut s = Span::new("r\"John\"");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("John".to_string())
        );

        let mut s = Span::new("r#\"John\"#");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("John".to_string())
        );

        let mut s = Span::new("r##\"John\"##");
        assert_eq!(
            *parse_string(&mut s).unwrap().expr,
            Expr::String("John".to_string())
        );
    }
}
