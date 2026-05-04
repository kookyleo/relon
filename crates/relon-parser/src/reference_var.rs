use crate::expr::parse_expr;
use crate::{create_range, id::id, Expr, Node, RefBase, Span, TokenKey};
use winnow::ascii::dec_uint;
use winnow::combinator::{alt, repeat};
use winnow::prelude::*;
use winnow::stream::Location;
use winnow::token::literal;

/// Parse a reference variable like &root.a.b
pub fn parse_ref_var<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();

    let base = winnow::combinator::preceded(
        '&',
        alt((
            literal("root").value(RefBase::Root),
            literal("sibling").value(RefBase::Sibling),
            literal("uncle").value(RefBase::Uncle),
            literal("prev").value(RefBase::Prev),
            literal("next").value(RefBase::Next),
            literal("index").value(RefBase::Index),
            literal("this").value(RefBase::This),
        )),
    )
    .parse_next(input)?;

    // Optional path after base
    let path: Vec<TokenKey> = repeat(
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
