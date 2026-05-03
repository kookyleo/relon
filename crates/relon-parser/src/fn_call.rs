use crate::{create_range, id::id, soc0, CallArg, Expr, Node, Span};
use winnow::combinator::{delimited, opt, separated};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};

pub fn parse_call_arg<'a>(
    input: &mut Span<'a>,
    parse_expr: fn(&mut Span<'a>) -> ModalResult<Node>,
) -> ModalResult<CallArg> {
    let checkpoint = input.checkpoint();
    // Try to parse "id ="
    let name = opt((id, soc0, '=', soc0)).parse_next(input)?;
    if let Some((id, _, _, _)) = name {
        let value = parse_expr(input)?;
        Ok(CallArg {
            name: Some(id.0),
            value,
        })
    } else {
        input.reset(&checkpoint);
        let value = parse_expr(input)?;
        Ok(CallArg { name: None, value })
    }
}

// Circular dependency handled by late binding in expr.rs
pub fn parse_fn_call<'a>(
    input: &mut Span<'a>,
    parse_expr: fn(&mut Span<'a>) -> ModalResult<Node>,
) -> ModalResult<Node> {
    let start_offset = input.location();

    // 🚨 Support paths like math.abs
    let path = crate::var::parse_path.parse_next(input)?;

    let args: Vec<CallArg> = delimited(
        (soc0, '(', soc0),
        separated(
            0..,
            |i: &mut Span<'a>| parse_call_arg(i, parse_expr),
            (soc0, ',', soc0),
        )
        .verify(|args: &Vec<CallArg>| {
            let mut saw_named = false;
            for arg in args {
                if arg.name.is_some() {
                    saw_named = true;
                } else if saw_named {
                    return false;
                }
            }
            true
        }),
        (soc0, ')'),
    )
    .parse_next(input)?;

    let end_offset = input.location();

    Ok(Node::new(
        Expr::FnCall { path, args },
        create_range(input, start_offset, end_offset),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::parse_expr;

    #[test]
    fn test_fn_call() {
        let mut s = Span::new("f(true, false)");
        let node = parse_fn_call(&mut s, parse_expr).unwrap();
        if let Expr::FnCall { path, args } = *node.expr {
            assert_eq!(path[0].to_string_key(), "f");
            assert_eq!(args.len(), 2);
            assert!(args[0].name.is_none());
            assert!(args[1].name.is_none());
        } else {
            panic!()
        }
    }

    #[test]
    fn test_fn_call_named() {
        let mut s = Span::new("f(a=1, b=2)");
        let node = parse_fn_call(&mut s, parse_expr).unwrap();
        if let Expr::FnCall { path, args } = *node.expr {
            assert_eq!(path[0].to_string_key(), "f");
            assert_eq!(args.len(), 2);
            assert_eq!(args[0].name.as_deref(), Some("a"));
            assert_eq!(args[1].name.as_deref(), Some("b"));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_fn_call_mixed() {
        let mut s = Span::new("f(1, b=2)");
        let node = parse_fn_call(&mut s, parse_expr).unwrap();
        if let Expr::FnCall { path: _, args } = *node.expr {
            assert_eq!(args.len(), 2);
            assert!(args[0].name.is_none());
            assert_eq!(args[1].name.as_deref(), Some("b"));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_fn_call_invalid_order() {
        let mut s = Span::new("f(a=1, 2)");
        assert!(parse_fn_call(&mut s, parse_expr).is_err());
    }

    #[test]
    fn test_fn_call_recursive() {
        let mut s = Span::new("f(a, b=g(1))");
        let node = parse_fn_call(&mut s, parse_expr).unwrap();
        if let Expr::FnCall { path, args } = *node.expr {
            assert_eq!(path[0].to_string_key(), "f");
            assert_eq!(args.len(), 2);
            assert!(args[0].name.is_none());
            assert_eq!(args[1].name.as_deref(), Some("b"));
            if let Expr::FnCall {
                path: inner_path,
                args: inner_args,
            } = &*args[1].value.expr
            {
                assert_eq!(inner_path[0].to_string_key(), "g");
                assert_eq!(inner_args.len(), 1);
            } else {
                panic!("Expected inner fn call")
            }
        } else {
            panic!()
        }
    }
}
