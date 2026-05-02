use crate::{
    create_range, decorator::parse_decorators, expr::parse_expr, id::id,
    prim::string::parse_string, soc0, Expr, Node, Span, TokenKey,
};
use winnow::combinator::{alt, delimited, opt, separated};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};

pub fn parse_dict<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();

    let pairs = delimited(
        ("{", soc0),
        separated(0.., parse_pair, (soc0, ",", soc0)),
        (soc0, opt(","), soc0, "}"),
    )
    .parse_next(input)?;

    let end = input.checkpoint();
    Ok(Node::new(
        Expr::Dict(pairs),
        create_range(input.offset_from(&start), input.offset_from(&end)),
    ))
}

pub(crate) fn parse_pair<'a>(input: &mut Span<'a>) -> ModalResult<(TokenKey, Node)> {
    let start = input.checkpoint();

    // Check for spread operator first: { ...base }
    let checkpoint = input.checkpoint();
    if (soc0, "...").parse_next(input).is_ok() {
        let base = parse_expr.parse_next(input)?;
        return Ok((
            TokenKey::Spread(create_range(
                input.offset_from(&start),
                input.offset_from(&start) + 3,
            )),
            base,
        ));
    }
    input.reset(&checkpoint);

    let decs_before_key = parse_decorators.parse_next(input)?;
    soc0.parse_next(input)?;

    let key = alt((
        id.map(|i| TokenKey::String(i.0, i.1)),
        parse_string.map(|node| {
            if let Expr::String(s) = *node.expr {
                TokenKey::String(s, node.range)
            } else {
                unreachable!()
            }
        }),
    ))
    .parse_next(input)?;

    (soc0, ":", soc0).parse_next(input)?;

    let decs_after_colon = parse_decorators.parse_next(input)?;
    soc0.parse_next(input)?;

    let mut value = parse_expr.parse_next(input)?;
    let mut all_decs = decs_before_key;
    all_decs.extend(decs_after_colon);
    value = value.with_decorators(all_decs);

    Ok((key, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dict_spread() {
        let mut s = Span::new("{ a: 1, ...base }");
        let node = parse_dict(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 2);
            if let TokenKey::Spread(_) = pairs[1].0 {
            } else {
                panic!("Expected spread key")
            }
        } else {
            panic!()
        }
    }
}
