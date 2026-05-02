pub mod decorator;
pub mod expr;
pub mod fmt_string;
pub mod fn_call;
pub mod id;
pub mod prim;
pub mod reference_var;
pub mod structure;
pub mod token;
pub mod var;

pub use token::*;

use winnow::ascii::multispace1;
use winnow::combinator::{alt, repeat};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};

use crate::prim::boolean::parse_bool;
use crate::prim::null::parse_null;
use crate::prim::number::parse_number;
use crate::prim::string::parse_string;

pub type Span<'a> = winnow::LocatingSlice<&'a str>;

/// Parse zero or more spaces or comments.
pub fn soc0<'a>(input: &mut Span<'a>) -> ModalResult<Vec<&'a str>> {
    repeat(
        0..,
        alt((multispace1.map(|s: &str| s), comment.map(|s: &str| s))),
    )
    .parse_next(input)
}

/// Parse single-line or multi-line comments.
pub fn comment<'a>(input: &mut Span<'a>) -> ModalResult<&'a str> {
    alt((line_comment, block_comment)).parse_next(input)
}

fn line_comment<'a>(input: &mut Span<'a>) -> ModalResult<&'a str> {
    ("//", winnow::token::take_till(0.., ('\n', '\r')))
        .map(|(_, s)| s)
        .parse_next(input)
}

fn block_comment<'a>(input: &mut Span<'a>) -> ModalResult<&'a str> {
    ("/*", winnow::token::take_until(0.., "*/"), "*/")
        .map(|(_, s, _)| s)
        .parse_next(input)
}

pub fn create_range(start_offset: usize, end_offset: usize) -> TokenRange {
    TokenRange {
        start: TokenPosition {
            offset: start_offset,
            ..Default::default()
        },
        end: TokenPosition {
            offset: end_offset,
            ..Default::default()
        },
    }
}

pub fn parse_prim<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    alt((parse_null, parse_bool, parse_number, parse_string)).parse_next(input)
}

/// Parse the root base which consists of optional decorators and a root List or Dict.
pub fn parse_base<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();
    let decorators = decorator::parse_decorators(input)?;
    soc0(input)?;

    let root = alt((structure::dict::parse_dict, structure::list::parse_list)).parse_next(input)?;

    let end = input.checkpoint();
    let range = create_range(input.offset_from(&start), input.offset_from(&end));

    Ok(Node {
        expr: root.expr,
        decorators,
        range,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_comments() {
        let mut s = Span::new(
            r##"/* hello world */
// this is a test file
{}"##,
        );
        let node = parse_base(&mut s).unwrap();
        assert!(matches!(*node.expr, Expr::Dict(_)));
    }

    #[test]
    fn test_parse_prim() {
        let mut s = Span::new("true");
        assert!(matches!(
            *parse_prim(&mut s).unwrap().expr,
            Expr::Bool(true)
        ));

        let mut s = Span::new("null");
        assert!(matches!(*parse_prim(&mut s).unwrap().expr, Expr::Null));

        let mut s = Span::new("1");
        assert!(matches!(*parse_prim(&mut s).unwrap().expr, Expr::Int(1)));

        let mut s = Span::new("\"foo\"");
        assert!(matches!(*parse_prim(&mut s).unwrap().expr, Expr::String(_)));
    }

    #[test]
    fn test_simple_root() {
        let mut s = Span::new(r#"{ "a": 1 }"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 1);
        } else {
            panic!()
        }

        let mut s = Span::new("// comment \n {foo: 1, bar: 2,}");
        let node = parse_base(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 2);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_expr_integration() {
        let mut s = Span::new(r#"{ "a": 1 != 2 }"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert!(matches!(*pairs[0].1.expr, Expr::Binary(Operator::Ne, _, _)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_comment_decorator_integration() {
        let mut s = Span::new(
            r###"
                // foo decorator
                @foo
                { "a": 1 }"###,
        );
        let node = parse_base(&mut s).unwrap();
        assert_eq!(node.decorators.len(), 1);
        assert_eq!(node.decorators[0].path[0].to_string_key(), "foo");
    }

    #[test]
    fn test_list_integration() {
        let mut s = Span::new(r#"[1, 2, 3]"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::List(elements) = *node.expr {
            assert_eq!(elements.len(), 3);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_ref_dict() {
        let mut s = Span::new(r#"{ "a": &sibling.b, "b": 2 }"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs.len(), 2);
            assert!(matches!(
                *pairs[0].1.expr,
                Expr::Reference {
                    base: RefBase::Sibling,
                    ..
                }
            ));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_ref_list() {
        let mut s = Span::new(r#"[&sibling.b[1], 2]"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::List(elements) = *node.expr {
            assert_eq!(elements.len(), 2);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_var_list() {
        let mut s = Span::new(r#"[a, 2]"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::List(elements) = *node.expr {
            assert!(matches!(*elements[0].expr, Expr::Variable(_)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_fn_call_list() {
        let mut s = Span::new(r#"[f({a: 1}), 2]"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::List(elements) = *node.expr {
            assert!(matches!(*elements[0].expr, Expr::FnCall { .. }));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_fmt_string_list() {
        let mut s = Span::new(r#"[f"a ${ &sibling.b[1] }", "b"]"#);
        let node = parse_base(&mut s).unwrap();
        if let Expr::List(elements) = *node.expr {
            assert!(matches!(*elements[0].expr, Expr::FString(_)));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_root_ref_in_fmt_string_dict() {
        let mut s = Span::new(r#"{ "a": f"a ${ &root.b[0] }", "b": [0, 1] }"#);
        let _node = parse_base(&mut s).unwrap();
        assert!(parse_base(&mut Span::new(
            r#"{ "a": f"a ${ &root.b[0] }", "b": [0, 1] }"#
        ))
        .is_ok());
    }

    #[test]
    fn test_soc0() {
        let mut s = Span::new("  // comment\n  /* block */  ");
        let res = soc0(&mut s).unwrap();
        assert_eq!(res.len(), 5); // space, comment, space, block, space
    }

    #[test]
    fn test_comments_detailed() {
        let mut s = Span::new("// line comment\n");
        assert_eq!(comment(&mut s).unwrap(), " line comment");

        let mut s = Span::new("/* block comment */");
        assert_eq!(comment(&mut s).unwrap(), " block comment ");
    }
}
