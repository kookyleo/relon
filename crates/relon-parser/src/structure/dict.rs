use crate::{
    create_range, decorator::parse_decorators, expr::parse_expr, id::id,
    prim::string::parse_string, soc0, Expr, Node, Span, TokenKey,
};
use winnow::combinator::{alt, delimited, opt, separated};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};

pub fn parse_dict<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();

    let pairs = delimited(
        ("{", soc0),
        separated(0.., parse_pair, (soc0, ",", soc0)),
        (soc0, opt(","), soc0, "}"),
    )
    .parse_next(input)?;

    let end_offset = input.location();
    Ok(Node::new(
        Expr::Dict(pairs),
        create_range(input, start_offset, end_offset),
    ))
}

pub(crate) fn parse_pair<'a>(input: &mut Span<'a>) -> ModalResult<(TokenKey, Node)> {
    // Check for spread operator first: { ...base }
    let checkpoint = input.checkpoint();
    soc0.parse_next(input)?;
    let spread_start_offset = input.location();
    if winnow::token::literal::<_, _, winnow::error::ContextError>("...")
        .parse_next(input)
        .is_ok()
    {
        let spread_end_offset = input.location();
        let base = parse_expr.parse_next(input)?;
        return Ok((
            TokenKey::Spread(create_range(input, spread_start_offset, spread_end_offset)),
            base,
        ));
    }
    input.reset(&checkpoint);

    let decs_before_key = parse_decorators.parse_next(input)?;
    soc0.parse_next(input)?;

    // Try parsing type hint (optional)
    let type_checkpoint = input.checkpoint();
    let type_hint = crate::expr::parse_type_node.parse_next(input).ok();

    // Now try to parse the key. If we successfully parsed a type hint but the key parsing fails,
    // or if the key parsing succeeds but there's no colon or parenthesis, we might have parsed
    // the key itself as the type hint!

    // Helper to parse key
    fn parse_key<'a>(i: &mut Span<'a>) -> ModalResult<TokenKey> {
        soc0.parse_next(i)?;
        alt((
            id.map(|i| TokenKey::String(i.0, i.1, false)),
            parse_string.map(|node| {
                if let Expr::String(s) = *node.expr {
                    TokenKey::String(s, node.range, false)
                } else {
                    unreachable!()
                }
            }),
            delimited("[", parse_expr, "]").map(|node| TokenKey::Dynamic(node, false)),
        ))
        .parse_next(i)
    }

    let mut parsed_type_hint = None;
    let mut parsed_key = None;

    if let Some(t) = type_hint {
        if let Ok(k) = parse_key.parse_next(input) {
            // We have both Type and Key. Now we expect either '(' or ':'
            soc0.parse_next(input)?;
            let peek = input.as_ref().chars().next();
            if peek == Some(':') || peek == Some('(') {
                parsed_type_hint = Some(t);
                parsed_key = Some(k);
            }
        }
    }

    if parsed_key.is_none() {
        // Fallback: The type_hint was actually the key, or there was no type hint.
        input.reset(&type_checkpoint);
        parsed_key = Some(parse_key.parse_next(input)?);
    }

    let key = parsed_key.unwrap();
    soc0.parse_next(input)?;

    // Check for method shorthand: `Key (params) : Expr`
    let is_method = winnow::token::literal::<_, _, winnow::error::ContextError>("(")
        .parse_next(input)
        .is_ok();

    let mut params = Vec::new();
    if is_method {
        params = separated(0.., crate::expr::parse_closure_param, (soc0, ",", soc0))
            .parse_next(input)?;
        let _ = (soc0, ")").parse_next(input)?;
    }

    (soc0, ":", soc0).parse_next(input)?;

    let decs_after_colon = parse_decorators.parse_next(input)?;
    soc0.parse_next(input)?;

    let value_start = input.location();
    let mut value = parse_expr.parse_next(input)?;
    let value_end = input.location();

    if is_method {
        // Desugar into a closure
        let closure_expr = Expr::Closure {
            params,
            return_type: parsed_type_hint.clone(),
            body: value,
        };
        value = Node::new(closure_expr, create_range(input, value_start, value_end));
    } else {
        // It's a regular field, apply type hint
        if parsed_type_hint.is_some() {
            value = value.with_type_hint(parsed_type_hint);
        }
    }

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
