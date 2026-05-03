use crate::{create_range, Expr, Node, Span};
use winnow::combinator::alt;
use winnow::prelude::*;
use winnow::stream::Location;
use winnow::token::literal;

/// Parse boolean literals 'true' and 'false'.
pub fn parse_bool<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    let v = alt((literal("true").value(true), literal("false").value(false))).parse_next(input)?;
    let end_offset = input.location();

    Ok(Node::new(
        Expr::Bool(v),
        create_range(input, start_offset, end_offset),
    ))
}
