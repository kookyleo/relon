pub mod decorator;
pub mod expr;
pub mod fmt_string;
pub mod fn_call;
pub mod id;
pub mod prim;
pub mod reference_var;
pub mod source;
pub mod structure;
pub mod token;
pub mod var;

pub use expr::child_nodes;
pub use token::*;

use winnow::ascii::{multispace0, multispace1};
use winnow::combinator::{alt, repeat};
use winnow::prelude::*;
use winnow::stream::{Location, Stream};

use crate::prim::boolean::parse_bool;
use crate::prim::null::parse_null;
use crate::prim::number::parse_number;
use crate::prim::string::parse_string;

pub type Span<'a> = winnow::LocatingSlice<&'a str>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseDocumentError {
    Parse { offset: usize, message: String },
    TrailingInput { offset: usize, remaining: String },
}

impl std::fmt::Display for ParseDocumentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { message, .. } => write!(f, "parse error: {message}"),
            Self::TrailingInput { offset, remaining } => {
                write!(f, "trailing input at byte {offset}: {remaining:?}")
            }
        }
    }
}

impl std::error::Error for ParseDocumentError {}

impl ParseDocumentError {
    pub fn source_span(&self) -> Option<miette::SourceSpan> {
        match self {
            Self::Parse { offset, .. } => Some((*offset, 1).into()),
            Self::TrailingInput { offset, remaining } => {
                Some((*offset, remaining.len().max(1)).into())
            }
        }
    }
}

pub fn parse_document(source: &str) -> Result<Node, ParseDocumentError> {
    let mut input = Span::new(source);
    let node = parse_base(&mut input).map_err(|error| ParseDocumentError::Parse {
        offset: input.location(),
        message: format!("{error:?}"),
    })?;
    soc0(&mut input).map_err(|error| ParseDocumentError::Parse {
        offset: input.location(),
        message: format!("{error:?}"),
    })?;
    if input.is_empty() {
        Ok(node)
    } else {
        let remaining = input.to_string();
        let remaining = remaining.chars().take(64).collect();
        Err(ParseDocumentError::TrailingInput {
            offset: input.location(),
            remaining,
        })
    }
}

/// Parse zero or more spaces or comments.
pub fn soc0<'a>(input: &mut Span<'a>) -> ModalResult<Vec<&'a str>> {
    repeat(
        0..,
        alt((multispace1.map(|s: &str| s), comment.map(|s: &str| s))),
    )
    .parse_next(input)
}

/// Parse zero or more spaces (no comments).
pub fn ws0<'a>(input: &mut Span<'a>) -> ModalResult<Vec<&'a str>> {
    repeat(0.., multispace1.map(|s: &str| s)).parse_next(input)
}

/// Extract leading comments as a single doc string. Consumes all preceding
/// spaces and comments up to the next non-trivia token.
pub fn parse_leading_comments<'a>(input: &mut Span<'a>) -> ModalResult<Option<String>> {
    let mut comments = Vec::new();
    loop {
        let _ = multispace0.parse_next(input)?;
        let checkpoint = input.checkpoint();
        if let Ok(c) = comment.parse_next(input) {
            comments.push(c.trim().to_string());
        } else {
            input.reset(&checkpoint);
            break;
        }
    }
    if comments.is_empty() {
        Ok(None)
    } else {
        Ok(Some(comments.join("\n")))
    }
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

pub fn create_range(input: &Span<'_>, start_offset: usize, end_offset: usize) -> TokenRange {
    TokenRange {
        start: position_at(input, start_offset),
        end: position_at(input, end_offset),
    }
}

pub fn combine_ranges(start: TokenRange, end: TokenRange) -> TokenRange {
    TokenRange {
        start: start.start,
        end: end.end,
    }
}

fn position_at(input: &Span<'_>, offset: usize) -> TokenPosition {
    let mut full_input = *input;
    full_input.reset_to_start();
    let source = *full_input.as_ref();
    position_at_source(source, offset)
}

fn position_at_source(source: &str, offset: usize) -> TokenPosition {
    let offset = offset.min(source.len());
    let end = if source.is_char_boundary(offset) {
        offset
    } else {
        let mut boundary = offset;
        while boundary > 0 && !source.is_char_boundary(boundary) {
            boundary -= 1;
        }
        boundary
    };

    let mut line = 1u32;
    let mut column = 1usize;
    let mut chars = source[..end].chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                line += 1;
                column = 1;
            }
            '\n' => {
                line += 1;
                column = 1;
            }
            _ => column += 1,
        }
    }

    TokenPosition {
        line,
        column,
        offset,
    }
}

pub fn parse_prim<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    alt((parse_null, parse_bool, parse_number, parse_string)).parse_next(input)
}

/// Parse the root base which consists of optional decorators and a root List or Dict.
pub fn parse_base<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let doc_comment = parse_leading_comments(input)?;
    let decorators = decorator::parse_decorators(input)?;
    soc0(input)?;
    let start_offset = decorators
        .first()
        .map(|decorator| decorator.range.start.offset)
        .unwrap_or_else(|| input.location());

    let root = alt((structure::dict::parse_dict, structure::list::parse_list)).parse_next(input)?;

    let end_offset = input.location();
    let range = create_range(input, start_offset, end_offset);

    Ok(Node {
        id: NodeId::alloc(),
        expr: root.expr,
        decorators,
        type_hint: None,
        range,
        doc_comment,
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
    fn test_parse_document_accepts_trailing_trivia() {
        assert!(parse_document("{ a: 1 } // trailing\n /* ok */").is_ok());
    }

    #[test]
    fn test_parse_document_rejects_trailing_tokens() {
        let err = parse_document("{ a: 1 } true").unwrap_err();
        assert!(matches!(
            err,
            ParseDocumentError::TrailingInput {
                offset: 9,
                ref remaining
            } if remaining == "true"
        ));
        assert_eq!(err.source_span(), Some((9, 4).into()));
    }

    #[test]
    fn test_parse_document_reports_parse_error_span() {
        let err = parse_document("{ a: }").unwrap_err();
        assert!(matches!(err, ParseDocumentError::Parse { .. }));
        assert!(err.source_span().is_some());
    }

    #[test]
    fn test_token_range_has_line_and_column() {
        let node = parse_document("// leading\n{\n  answer: 42\n}\n").unwrap();
        assert_eq!(node.range.start.line, 2);
        assert_eq!(node.range.start.column, 1);
        assert_eq!(node.range.end.line, 4);
        assert_eq!(node.range.end.column, 2);

        if let Expr::Dict(pairs) = *node.expr {
            let TokenKey::String(_, key_range, _) = &pairs[0].0 else {
                panic!("Expected string key")
            };
            assert_eq!(key_range.start.line, 3);
            assert_eq!(key_range.start.column, 3);
            assert_eq!(pairs[0].1.range.start.line, 3);
            assert_eq!(pairs[0].1.range.start.column, 11);
        } else {
            panic!("Expected dict")
        }
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
    fn test_doc_comment_extraction() {
        let src = r#"{
            // line 1
            // line 2
            a: 1,
            /* block */
            b: 2
        }"#;
        let node = parse_document(src).unwrap();
        if let Expr::Dict(pairs) = *node.expr {
            assert_eq!(pairs[0].1.doc_comment.as_deref(), Some("line 1\nline 2"));
            assert_eq!(pairs[1].1.doc_comment.as_deref(), Some("block"));
        } else {
            panic!()
        }
    }

    #[test]
    fn test_comments_detailed() {
        let mut s = Span::new("// line comment\n");
        assert_eq!(comment(&mut s).unwrap(), " line comment");

        let mut s = Span::new("/* block comment */");
        assert_eq!(comment(&mut s).unwrap(), " block comment ");
    }
}
