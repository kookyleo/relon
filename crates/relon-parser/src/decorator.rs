use crate::expr::parse_expr;
use crate::fn_call::parse_call_arg;
use crate::{create_range, soc0, CallArg, Decorator, Span};
use winnow::combinator::{delimited, opt, preceded, repeat, separated};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};

pub fn parse_decorators<'a>(input: &mut Span<'a>) -> ModalResult<Vec<Decorator>> {
    repeat(0.., preceded(soc0, decorator)).parse_next(input)
}

fn decorator<'a>(input: &mut Span<'a>) -> ModalResult<Decorator> {
    let start = input.checkpoint();
    let (path, args) = preceded(
        '@',
        (
            crate::var::parse_path,
            opt(delimited(
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
            )),
        ),
    )
    .parse_next(input)?;

    let end = input.checkpoint();
    Ok(Decorator {
        path,
        args: args.unwrap_or_default(),
        range: create_range(input.offset_from(&start), input.offset_from(&end)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decorator() {
        let mut s = Span::new("@foo");
        let decs = parse_decorators(&mut s).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].path[0].to_string_key(), "foo");

        let mut s = Span::new("@foo(true, false)");
        let decs = parse_decorators(&mut s).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].args.len(), 2);
    }

    #[test]
    fn test_decorator_named() {
        let mut s = Span::new("@foo(a=true, b=false)");
        let decs = parse_decorators(&mut s).unwrap();
        assert_eq!(decs.len(), 1);
        assert_eq!(decs[0].args.len(), 2);
        assert_eq!(decs[0].args[0].name.as_deref(), Some("a"));
    }
}
