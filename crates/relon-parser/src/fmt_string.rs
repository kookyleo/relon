use crate::expr::parse_expr_zone;
use crate::prim::string::{parse_escaped_char, parse_escaped_whitespace};
use crate::{create_range, Expr, FStringPart, Node, Span};
use winnow::combinator::{alt, delimited, preceded, repeat};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};
use winnow::token::{any, literal, take_till, take_while};

pub fn parse_fmt_string<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();

    let parts = preceded('f', alt((normal_fmt_string, raw_fmt_string))).parse_next(input)?;

    let end_offset = input.location();
    Ok(Node::new(
        Expr::FString(parts),
        create_range(input, start_offset, end_offset),
    ))
}

fn normal_fmt_string<'a>(input: &mut Span<'a>) -> ModalResult<Vec<FStringPart>> {
    delimited(
        '"',
        repeat(
            0..,
            alt((
                parse_expr_zone.map(|node| FStringPart::Interpolation(Box::new(node))),
                parse_escaped_char.map(|c| FStringPart::Literal(c.to_string())),
                parse_escaped_whitespace.value(FStringPart::Literal("".to_string())),
                take_till(1.., ('"', '\\', '$')).map(|s: &str| FStringPart::Literal(s.to_string())),
                ('$').map(|c: char| FStringPart::Literal(c.to_string())),
            )),
        )
        .map(merge_parts),
        '"',
    )
    .parse_next(input)
}

fn raw_fmt_string<'a>(input: &mut Span<'a>) -> ModalResult<Vec<FStringPart>> {
    let hashes: &str = take_while(0.., '#').parse_next(input)?;
    let hash_count = hashes.len();

    literal('"').parse_next(input)?;

    let mut closing = String::from("\"");
    for _ in 0..hash_count {
        closing.push('#');
    }

    let mut parts = Vec::new();
    loop {
        if input.is_empty() {
            return Err(winnow::error::ErrMode::Backtrack(
                winnow::error::ContextError::default(),
            ));
        }
        if input.starts_with(closing.as_str()) {
            break;
        }

        if input.starts_with("${") {
            let checkpoint = input.checkpoint();
            if let Ok(node) = parse_expr_zone.parse_next(input) {
                parts.push(FStringPart::Interpolation(Box::new(node)));
                continue;
            } else {
                input.reset(&checkpoint);
            }
        }

        let c: char = any.parse_next(input)?;
        parts.push(FStringPart::Literal(c.to_string()));
    }

    literal(closing.as_str()).parse_next(input)?;
    Ok(merge_parts(parts))
}

fn merge_parts(parts: Vec<FStringPart>) -> Vec<FStringPart> {
    let mut merged = Vec::new();
    for part in parts {
        match part {
            FStringPart::Literal(s) if s.is_empty() => continue,
            FStringPart::Literal(s) => {
                if let Some(FStringPart::Literal(ref mut last)) = merged.last_mut() {
                    last.push_str(&s);
                } else {
                    merged.push(FStringPart::Literal(s));
                }
            }
            FStringPart::Interpolation(node) => merged.push(FStringPart::Interpolation(node)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_string() {
        let mut s = Span::new("f\"hello ${name}\"");
        let node = parse_fmt_string(&mut s).unwrap();
        if let Expr::FString(parts) = *node.expr {
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0], FStringPart::Literal("hello ".to_string()));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_raw_fmt_string() {
        let mut s = Span::new("f#\"hello ${name}\"#");
        let node = parse_fmt_string(&mut s).unwrap();
        if let Expr::FString(parts) = *node.expr {
            assert_eq!(parts.len(), 2);
            assert_eq!(parts[0], FStringPart::Literal("hello ".to_string()));
        } else {
            panic!()
        }
    }
}
