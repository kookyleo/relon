use crate::{
    create_range, decorator::parse_decorators, expr::parse_expr, id::id, soc0, Expr, Node, Span,
};
use winnow::combinator::{delimited, opt, preceded, separated};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};

pub fn parse_list<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();

    // Try comprehension first
    let checkpoint = input.checkpoint();
    if let Ok(node) = parse_comprehension.parse_next(input) {
        return Ok(node);
    }
    input.reset(&checkpoint);

    let elements = delimited(
        ("[", soc0),
        separated(0.., parse_element, (soc0, ",", soc0)),
        (soc0, opt(","), soc0, "]"),
    )
    .parse_next(input)?;

    let end = input.checkpoint();
    Ok(Node::new(
        Expr::List(elements),
        create_range(input.offset_from(&start), input.offset_from(&end)),
    ))
}

fn parse_element<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let decorators = parse_decorators.parse_next(input)?;

    // Check for spread operator
    let start = input.checkpoint();
    let checkpoint = input.checkpoint();
    if (soc0, "...").parse_next(input).is_ok() {
        let inner = parse_expr.parse_next(input)?;
        let end = input.checkpoint();
        Ok(Node::new(
            Expr::Spread(inner),
            create_range(input.offset_from(&start), input.offset_from(&end)),
        ))
    } else {
        input.reset(&checkpoint);
        soc0.parse_next(input)?;
        let node = parse_expr.parse_next(input)?;
        Ok(node.with_decorators(decorators))
    }
}

fn parse_comprehension<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();

    let (element, _, target_id, _, iterable, condition) = delimited(
        ("[", soc0),
        (
            parse_expr,
            (soc0, "for", soc0),
            id,
            (soc0, "in", soc0),
            parse_expr,
            opt(preceded((soc0, "if", soc0), parse_expr)),
        ),
        (soc0, "]"),
    )
    .parse_next(input)?;

    let end = input.checkpoint();
    Ok(Node::new(
        Expr::Comprehension {
            element,
            id: target_id.0,
            iterable,
            condition,
        },
        create_range(input.offset_from(&start), input.offset_from(&end)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_spread() {
        let mut s = Span::new("[1, ...others, 2]");
        let node = parse_list(&mut s).unwrap();
        if let Expr::List(elements) = *node.expr {
            assert_eq!(elements.len(), 3);
            if let Expr::Spread(_) = *elements[1].expr {
            } else {
                panic!("Expected spread")
            }
        } else {
            panic!()
        }
    }

    #[test]
    fn test_comprehension() {
        let mut s = Span::new("[x * 2 for x in my_list if x > 0]");
        let node = parse_list(&mut s).unwrap();
        if let Expr::Comprehension { id, .. } = *node.expr {
            assert_eq!(id, "x");
        } else {
            panic!()
        }
    }
}
