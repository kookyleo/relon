use crate::{create_range, id::id, prim::string::parse_string, Expr, Node, Span, TokenKey};
use winnow::ascii::dec_uint;
use winnow::combinator::{alt, delimited, preceded, repeat};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};

/// Parse a variable or path access.
pub fn parse_var<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();
    let path = parse_path.parse_next(input)?;
    let end = input.checkpoint();
    Ok(Node::new(
        Expr::Variable(path),
        create_range(input.offset_from(&start), input.offset_from(&end)),
    ))
}

pub fn parse_path<'a>(input: &mut Span<'a>) -> ModalResult<Vec<TokenKey>> {
    let head = id.parse_next(input)?;
    let mut path = vec![TokenKey::String(head.0, head.1)];

    let rest: Vec<TokenKey> = repeat(
        0..,
        alt((
            preceded(
                ".",
                alt((
                    dec_uint.map(TokenKey::Index),
                    id.map(|i| TokenKey::String(i.0, i.1)),
                )),
            ),
            delimited(
                "[",
                alt((
                    dec_uint.map(TokenKey::Index),
                    parse_string.map(|node| {
                        if let Expr::String(s) = *node.expr {
                            TokenKey::String(s, node.range)
                        } else {
                            unreachable!()
                        }
                    }),
                )),
                "]",
            ),
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
                TokenKey::String(s, _) => assert_eq!(s, "a"),
                _ => panic!(),
            }
            match &path[1] {
                TokenKey::String(s, _) => assert_eq!(s, "b"),
                _ => panic!(),
            }
            match &path[2] {
                TokenKey::Index(i) => assert_eq!(*i, 0),
                _ => panic!(),
            }
        } else {
            panic!()
        }
    }
}
