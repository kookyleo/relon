use crate::{
    create_range, id::id, prim::string::parse_string, Expr, Node, RefBase, Span, TokenKey,
};
use winnow::ascii::dec_uint;
use winnow::combinator::{alt, delimited, preceded, repeat};
use winnow::prelude::*;
use winnow::stream::Location;
use winnow::token::literal;

/// Parse a reference variable like &root.a.b
pub fn parse_ref_var<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();

    let base = preceded(
        '&',
        alt((
            literal("root").value(RefBase::Root),
            literal("sibling").value(RefBase::Sibling),
            literal("uncle").value(RefBase::Uncle),
        )),
    )
    .parse_next(input)?;

    // Optional path after base
    let path: Vec<TokenKey> = repeat(
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

    let end_offset = input.location();
    Ok(Node::new(
        Expr::Reference { base, path },
        create_range(input, start_offset, end_offset),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ref_var() {
        let mut s = Span::new("&root.a");
        let node = parse_ref_var(&mut s).unwrap();
        if let Expr::Reference { base, path } = *node.expr {
            assert_eq!(base, RefBase::Root);
            assert_eq!(path.len(), 1);
        } else {
            panic!()
        }
    }
}
