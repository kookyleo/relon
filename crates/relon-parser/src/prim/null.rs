use crate::{create_range, Expr, Node, Span};
use winnow::prelude::*;
use winnow::stream::Location;
use winnow::token::literal;

/// Parse the 'null' literal.
pub fn parse_null<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start_offset = input.location();
    literal("null").parse_next(input)?;
    let end_offset = input.location();

    Ok(Node::new(
        Expr::Null,
        create_range(input, start_offset, end_offset),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null() {
        let mut s = Span::new("null");
        let node = parse_null(&mut s).unwrap();
        assert_eq!(*node.expr, Expr::Null);
    }
}
