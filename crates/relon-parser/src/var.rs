use crate::expr::parse_expr;
use crate::{create_range, id::id, Expr, Node, Span, TokenKey};
use winnow::ascii::dec_uint;
use winnow::combinator::{alt, repeat};
use winnow::prelude::*;
use winnow::stream::Location;
use winnow::token::literal;

/// Parse a variable or path access.
pub fn parse_var<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let path = parse_path.parse_next(input)?;
    let end_offset = input.location();
    Ok(Node::new(
        Expr::Variable(path),
        create_range(input, start_offset, end_offset),
    ))
}

pub fn parse_path<'a>(input: &mut Span<'a>) -> ModalResult<Vec<TokenKey>> {
    let head = id.parse_next(input)?;
    let mut path = vec![TokenKey::String(head.0, head.1, false)];

    let rest: Vec<TokenKey> = repeat(
        0..,
        alt((
            // Dot access: .a or ?.a
            (
                alt((literal("?.").value(true), literal(".").value(false))),
                alt((
                    dec_uint.map(|i| (Some(i), None)),
                    id.map(|i| (None, Some(i))),
                )),
            )
                .map(|(opt, (idx, name))| {
                    if let Some(i) = idx {
                        TokenKey::Index(i, opt)
                    } else {
                        let n = name.unwrap();
                        TokenKey::String(n.0, n.1, opt)
                    }
                }),
            // Bracket access: [expr] or ?[expr]
            (
                alt((literal("?[").value(true), literal("[").value(false))),
                parse_expr,
                "]",
            )
                .map(|(opt, expr, _)| TokenKey::Dynamic(expr, opt)),
        )),
    )
    .parse_next(input)?;

    path.extend(rest);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_var() {
        let mut s = Span::new("a.b[0]");
        let node = parse_var(&mut s).unwrap();
        if let Expr::Variable(path) = *node.expr {
            assert_eq!(path.len(), 3);
            match &path[0] {
                TokenKey::String(s, _, _) => assert_eq!(s, "a"),
                _ => panic!(),
            }
            match &path[1] {
                TokenKey::String(s, _, _) => assert_eq!(s, "b"),
                _ => panic!(),
            }
            match &path[2] {
                TokenKey::Dynamic(expr_node, _) => {
                    if let Expr::Int(i) = *expr_node.expr {
                        assert_eq!(i, 0);
                    } else {
                        panic!("Expected Int(0) in dynamic key");
                    }
                }
                _ => panic!("Expected dynamic key"),
            }
        } else {
            panic!()
        }
    }

    #[test]
    fn test_optional_bracket() {
        let mut s = Span::new("a?[0]");
        let node = parse_var(&mut s).unwrap();
        let Expr::Variable(path) = *node.expr else {
            panic!("not a variable")
        };
        assert_eq!(path.len(), 2);
        let TokenKey::Dynamic(_, opt) = &path[1] else {
            panic!("expected Dynamic, got {:?}", path[1]);
        };
        assert!(opt, "expected optional flag on dynamic key");
    }
}
