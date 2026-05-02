use crate::fn_call::parse_fn_call;
use crate::prim::{parse_bool, parse_null, parse_number, parse_string};
use crate::reference_var::parse_ref_var;
use crate::structure::dict::parse_dict;
use crate::structure::list::parse_list;
use crate::var::parse_var;
use crate::{create_range, soc0, Expr, Node, Operator, Span};
use winnow::combinator::{alt, delimited, preceded, repeat};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};
use winnow::token::literal;

pub fn parse_expr<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    parse_ternary(input)
}

// Level 9: Ternary (cond ? then : else)
fn parse_ternary<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();
    let cond = parse_pipe.parse_next(input)?;

    let checkpoint = input.checkpoint();
    if (soc0, '?', soc0).parse_next(input).is_ok() {
        let then = parse_expr.parse_next(input)?;
        let _ = (soc0, ':', soc0).parse_next(input)?;
        let els = parse_expr.parse_next(input)?;
        let end = input.checkpoint();
        Ok(Node::new(
            Expr::Ternary { cond, then, els },
            create_range(input.offset_from(&start), input.offset_from(&end)),
        ))
    } else {
        input.reset(&checkpoint);
        Ok(cond)
    }
}

// Level 8: Pipe (|)
fn parse_pipe<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_logic_or.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        ((soc0, "|", soc0).value(Operator::Pipe), parse_logic_or),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 7: Logic OR (||)
fn parse_logic_or<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_logic_and.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        ((soc0, "||", soc0).value(Operator::Or), parse_logic_and),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 6: Logic AND (&&)
fn parse_logic_and<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_comparison.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        ((soc0, "&&", soc0).value(Operator::And), parse_comparison),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 5: Comparison (==, !=, <, >, <=, >=)
fn parse_comparison<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_additive.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        (
            (
                soc0,
                alt((
                    literal("==").value(Operator::Eq),
                    literal("!=").value(Operator::Ne),
                    literal("<=").value(Operator::Le),
                    literal(">=").value(Operator::Ge),
                    literal("<").value(Operator::Lt),
                    literal(">").value(Operator::Gt),
                )),
                soc0,
            )
                .map(|(_, op, _)| op),
            parse_additive,
        ),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 4: Additive (+, -)
fn parse_additive<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_multiplicative.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        (
            (
                soc0,
                alt((
                    literal("+").value(Operator::Add),
                    literal("-").value(Operator::Sub),
                )),
                soc0,
            )
                .map(|(_, op, _)| op),
            parse_multiplicative,
        ),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 3: Multiplicative (*, /, %)
fn parse_multiplicative<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let left = parse_unary.parse_next(input)?;
    let rest: Vec<(Operator, Node)> = repeat(
        0..,
        (
            (
                soc0,
                alt((
                    literal("*").value(Operator::Mul),
                    literal("/").value(Operator::Div),
                    literal("%").value(Operator::Mod),
                )),
                soc0,
            )
                .map(|(_, op, _)| op),
            parse_unary,
        ),
    )
    .parse_next(input)?;
    Ok(fold_binary(left, rest))
}

// Level 2: Unary (!, -)
fn parse_unary<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();
    let checkpoint = input.checkpoint();

    fn parse_unary_op<'a>(i: &mut Span<'a>) -> ModalResult<Operator> {
        alt((
            literal("!").value(Operator::Not),
            literal("-").value(Operator::Sub),
        ))
        .parse_next(i)
    }

    if let Ok(op) = parse_unary_op.parse_next(input) {
        let node = parse_unary.parse_next(input)?;
        let end = input.checkpoint();
        Ok(Node::new(
            Expr::Unary(op, node),
            create_range(input.offset_from(&start), input.offset_from(&end)),
        ))
    } else {
        input.reset(&checkpoint);
        parse_atomic.parse_next(input)
    }
}

// Level 0: Atomic
fn parse_atomic<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    preceded(
        soc0,
        alt((
            parse_null,
            parse_bool,
            parse_number,
            parse_string,
            |i: &mut Span<'a>| crate::fmt_string::parse_fmt_string(i),
            parse_ref_var,
            |i: &mut Span<'a>| parse_fn_call(i, parse_expr),
            parse_var,
            parse_list,
            parse_dict,
            delimited("(", parse_expr, ")"),
        )),
    )
    .parse_next(input)
}

fn fold_binary(mut left: Node, rest: Vec<(Operator, Node)>) -> Node {
    for (op, right) in rest {
        let start_offset = left.range.start.offset;
        let end_offset = right.range.end.offset;
        left = Node::new(
            Expr::Binary(op, left, right),
            create_range(start_offset, end_offset),
        );
    }
    left
}

pub fn parse_expr_zone<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    delimited(("${", soc0), parse_expr, (soc0, "}")).parse_next(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group() {
        let mut s = Span::new("(1 + 2)");
        let node = parse_expr(&mut s).unwrap();
        match *node.expr {
            Expr::Binary(Operator::Add, _, _) => {}
            _ => panic!("Expected binary add"),
        }
    }

    #[test]
    fn test_atomic() {
        let mut s = Span::new("null");
        assert!(matches!(*parse_atomic(&mut s).unwrap().expr, Expr::Null));

        let mut s = Span::new("true");
        assert!(matches!(
            *parse_atomic(&mut s).unwrap().expr,
            Expr::Bool(true)
        ));

        let mut s = Span::new("123");
        assert!(matches!(
            *parse_atomic(&mut s).unwrap().expr,
            Expr::Int(123)
        ));

        let mut s = Span::new("\"hello\"");
        assert!(matches!(
            *parse_atomic(&mut s).unwrap().expr,
            Expr::String(_)
        ));
    }

    #[test]
    fn test_precedence() {
        let mut s = Span::new("1 + 2 * 3");
        let node = parse_expr(&mut s).unwrap();
        // Should be 1 + (2 * 3)
        if let Expr::Binary(Operator::Add, left, right) = *node.expr {
            assert!(matches!(*left.expr, Expr::Int(1)));
            assert!(matches!(*right.expr, Expr::Binary(Operator::Mul, _, _)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_parse_expr_zone() {
        let mut s = Span::new("${ 1 + 2 }");
        let node = parse_expr_zone(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Binary(Operator::Add, _, _)));
    }

    #[test]
    fn test_expr_ternary() {
        let mut s = Span::new("true ? 1 : 2");
        let node = parse_expr(&mut s).unwrap();
        if let Expr::Ternary { ref cond, .. } = *node.expr {
            assert!(matches!(*cond.expr, Expr::Bool(true)));
        } else {
            panic!()
        }

        let mut s = Span::new("true? 1:2");
        assert!(parse_expr(&mut s).is_ok());
    }

    #[test]
    fn test_unary() {
        let mut s = Span::new("!true");
        let node = parse_expr(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Unary(Operator::Not, _)));

        let mut s = Span::new("-1");
        let node = parse_expr(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Unary(Operator::Sub, _)));
    }

    #[test]
    fn test_complex_expr() {
        let mut s = Span::new("1 + f(2, 3) * var3");
        assert!(parse_expr(&mut s).is_ok());
    }

    #[test]
    fn test_expr_zone_with_comments() {
        let mut s = Span::new("${ /* comment */ 1 // line comment\n }");
        let node = parse_expr_zone(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Int(1)));
    }
}
