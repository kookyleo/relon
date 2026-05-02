use crate::{create_range, Expr, Node, Span};
use winnow::combinator::alt;
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};
use winnow::token::literal;

/// Parse boolean literals 'true' and 'false'.
pub fn parse_bool<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();
    let v = alt((literal("true").value(true), literal("false").value(false))).parse_next(input)?;
    let end = input.checkpoint();

    Ok(Node::new(
        Expr::Bool(v),
        create_range(input.offset_from(&start), input.offset_from(&end)),
    ))
}
