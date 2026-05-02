use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::str;

pub use decorator::Annotation;
pub use dict::EsonDict;
pub use dict::Key;
pub use expr::chunk::TokenChunk;
pub use fmt_string::FmtString;
pub use fn_call::EsonFnCall;
pub use list::EsonList;
use nom::{
    branch::alt,
    combinator::{map, opt},
    error::VerboseError,
    IResult,
    sequence::{delimited, preceded},
};
use nom::sequence::pair;
pub use reference::{EsonRef, RefIndex};
pub use var::EsonVar;

use crate::decorator::parse_decorators;
use crate::dict::parse_dict;
use crate::expr::parse_expr;
use crate::fmt_string::parse_fmt_string;
use crate::fn_call::parse_fn_call;
use crate::list::parse_lst;
use crate::reference::parse_ref;
use crate::sp::sp;
use crate::var::parse_var;

mod decorator;
mod comments;
mod dict;
mod expr;
mod fmt_string;
mod fn_call;
mod id;
mod list;
mod reference;
mod sp;
mod util_iter;
mod var;
mod token;
mod prim;

#[derive(Debug, PartialEq)]
pub enum Eson {
    Dict(Option<Vec<Annotation>>, HashMap<Key, EsonSegment>),
    List(Option<Vec<Annotation>>, Vec<EsonSegment>),
}

#[derive(Debug, PartialEq, Clone)]
pub enum EsonSegment {
    Null,
    Boolean(bool),
    Int(i64),
    Float(f64),
    String(String),
    FmtString(Vec<FmtString>),
    List(Vec<EsonSegment>),
    Dict(HashMap<Key, EsonSegment>),
    FnCall(String, Vec<EsonSegment>),
    Var(String),
    Ref(EsonRef),
    // eg. self, super, $, self.ele, super["ele"], $[0] ..
    Expr(TokenChunk),
}

impl Default for Eson {
    fn default() -> Self {
        Eson::Dict(None, HashMap::new())
    }
}

impl Default for EsonSegment {
    fn default() -> Self {
        EsonSegment::Null
    }
}

impl Display for EsonSegment {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            EsonSegment::Null => write!(f, "null"),
            EsonSegment::String(s) => write!(f, "{}", s),
            EsonSegment::Boolean(b) => write!(f, "{}", b),
            EsonSegment::Int(i) => write!(f, "{}", i),
            EsonSegment::Float(fl) => write!(f, "{}", fl),
            EsonSegment::FmtString(fs) => write!(f, "FmtString[..]"),
            EsonSegment::List(l) => write!(f, "List[..]"),
            EsonSegment::Dict(d) => write!(f, "Dict{{..}}"),
            EsonSegment::FnCall(name, args) => write!(f, "{}(..)", name),
            EsonSegment::Var(name) => write!(f, "Var({})", name),
            EsonSegment::Ref(r) => write!(f, "Ref(..)"),
            EsonSegment::Expr(e) => write!(f, "Expr(..)"),
        }
    }
}

impl From<Eson> for EsonSegment {
    fn from(e: Eson) -> EsonSegment {
        match e {
            Eson::Dict(_, d) => EsonSegment::Dict(d),
            Eson::List(_, l) => EsonSegment::List(l),
        }
    }
}

impl From<EsonSegment> for Eson {
    fn from(v: EsonSegment) -> Eson {
        match v {
            EsonSegment::Dict(d) => Eson::Dict(None, d),
            EsonSegment::List(l) => Eson::List(None, l),
            _ => panic!("Not a dict or list"),
        }
    }
}

pub fn eson_seg(i: &str) -> IResult<&str, EsonSegment, VerboseError<&str>> {
    preceded(
        sp,
        alt((
            map(parse_expr, |e| e.into()),
            map(parse_null, |n| n.into()),
            map(parse_fmt_string, |s| s.into()),
            map(parse_string, |s| s.into()),
            map(parse_boolean, |b| b.into()),
            map(parse_numeric, |n| n.into()), // include Int(i64) and Float(f64)
            map(parse_lst, |l| l.into()),
            map(parse_dict, |d| d.into()),
            map(parse_fn_call, |f| f.into()),
            map(parse_ref, |r| r.into()),
            map(parse_var, |v| v.into()),
        )),
    )(i)
}

/// the root element of a JSON parser is either a dict or a list
pub fn root(input: &str) -> IResult<&str, Eson, VerboseError<&str>> {
    delimited(
        sp,
        alt((
            map(pair(parse_decorators, parse_dict), |(a, d)| {
                Eson::Dict(a, d)
            }),
            map(pair(parse_decorators, parse_lst), |(a, l)| {
                Eson::List(a, l)
            }),
        )),
        opt(sp),
    )(input)
}

#[cfg(test)]
mod tests {
    use crate::dict::Key;
    use crate::EsonSegment::{Dict, List};
    use crate::expr::token_chunk;
    use crate::token::Token;
    use crate::var::EsonVar;

    use super::*;

    #[test]
    fn test_eson_seg() {
        let dat = r#"1 + 2"#;

        assert_eq!(token_chunk(dat), Ok(("", TokenChunk::from(
            vec![
                Token::Prim(EsonNumeric::Int(1).into()),
                Token::OpAdd,
                Token::Prim(EsonNumeric::Int(2).into()),
            ]
        ))));

        assert_eq!(eson_seg(dat), Ok(("", EsonSegment::Expr(TokenChunk::from(
            vec![
                Token::Prim(EsonNumeric::Int(1).into()),
                Token::OpAdd,
                Token::Prim(EsonNumeric::Int(2).into()),
            ]
        )))));

        assert_eq!(eson_seg("null"), Ok(("", EsonSegment::Null)));
    }

    #[test]
    fn test_root_dict() {
        let dat = r###"
        {
            // hello
            @hello
            "c": {},
            "d": "bar",
        }
        "###;

        assert_eq!(
            root(dat),
            Ok((
                "",
                Eson::Dict(
                    None,
                    vec![
                        (
                            Key::new("c", Some(vec![Annotation::new("hello", None)])),
                            Dict(HashMap::new())
                        ),
                        (
                            Key::new("d", Some(vec![Annotation::new("hello", None)])),
                            EsonSegment::String("bar".to_string())
                        ),
                    ]
                        .into_iter()
                        .collect(),
                ),
            ))
        );
    }

    #[test]
    fn test_root_lst() {
        assert_eq!(
            root(
                r###"
            @something
            [
                1,
                { name: foo, title: bar },
                "just a test"
            ]"###
            ),
            Ok((
                "",
                Eson::List(
                    Some(vec![Annotation::new("something", None)]),
                    vec![
                        EsonSegment::Int(1),
                        Dict(
                            vec![
                                (Key::new("name", None), EsonSegment::Var("foo".to_string())),
                                (Key::new("title", None), EsonSegment::Var("bar".to_string())),
                            ]
                                .into_iter()
                                .collect()
                        ),
                        EsonSegment::String("just a test".to_string()),
                    ],
                )
            ))
        );
    }

    #[test]
    fn test_root_dict_with_decorator() {
        let eson = r##"
        // comment1
        // comment2
        @hello
        @world("hello", 1)

        {
            // comment3
            "a": 42,
            "b": [
                "x", // comment4
                "y",
                12
            ],
            "c": {
                "hello": "world",
                "foo": r"bar",
                // comment5
                "bar": f"hello ${name}",
            }
        }
        "##;
        assert_eq!(
            root(eson),
            Ok((
                "",
                Eson::Dict(
                    Some(vec![
                        Annotation::new("hello", None),
                        Annotation::new(
                            "world",
                            Some(vec![
                                EsonSegment::String("hello".to_string()),
                                EsonSegment::Int(1),
                            ]),
                        ),
                    ]),
                    vec![
                        (Key::new("a", None), EsonSegment::Int(42)),
                        (
                            Key::new("b", None),
                            List(vec![
                                EsonSegment::String("x".to_string()),
                                EsonSegment::String("y".to_string()),
                                EsonSegment::Int(12),
                            ])
                        ),
                        (
                            Key::new("c", None),
                            Dict(
                                vec![
                                    (
                                        Key::new("hello", None),
                                        EsonSegment::String("world".to_string())
                                    ),
                                    (
                                        Key::new("foo", None),
                                        EsonSegment::String("bar".to_string())
                                    ),
                                    (
                                        Key::new("bar", None),
                                        EsonSegment::FmtString(vec![
                                            FmtString::Literal("hello ".to_string()),
                                            FmtString::Expr(TokenChunk::from(EsonVar::new("name"))),
                                        ])
                                    ),
                                ]
                                    .into_iter()
                                    .collect()
                            )
                        ),
                    ]
                        .into_iter()
                        .collect(),
                )
            ))
        );
    }

    #[test]
    fn test_root_dict_ref() {
        let dat = r#"{
            "name": "John",
            "ref_to_name": $["name"],
        }"#;
        assert_eq!(
            root(dat),
            Ok((
                "",
                Eson::Dict(
                    None,
                    vec![
                        (
                            Key::new("name", None),
                            EsonSegment::String("John".to_string())
                        ),
                        (
                            Key::new("ref_to_name", None),
                            EsonSegment::Ref(EsonRef::Root(vec![RefIndex::Str(
                                "name".to_string()
                            ), ]))
                        ),
                    ]
                        .into_iter()
                        .collect(),
                ),
            ))
        );

        let dat = r#"{
            "name": "John",
            "ref_to_name": self.name
        }"#;
        assert_eq!(
            root(dat),
            Ok((
                "",
                Eson::Dict(
                    None,
                    vec![
                        (
                            Key::new("name", None),
                            EsonSegment::String("John".to_string())
                        ),
                        (
                            Key::new("ref_to_name", None),
                            EsonSegment::Ref(EsonRef::Curr(vec![RefIndex::Str(
                                "name".to_string()
                            ), ]))
                        ),
                    ]
                        .into_iter()
                        .collect(),
                ),
            ))
        );

        let dat = r#"{
            "name": "John",
            "ref_to_name": super["name"]
        }"#;
        assert_eq!(
            root(dat),
            Ok((
                "",
                Eson::Dict(
                    None,
                    vec![
                        (
                            Key::new("name", None),
                            EsonSegment::String("John".to_string())
                        ),
                        (
                            Key::new("ref_to_name", None),
                            EsonSegment::Ref(EsonRef::Super(vec![RefIndex::Str(
                                "name".to_string()
                            ), ]))
                        ),
                    ]
                        .into_iter()
                        .collect(),
                ),
            ))
        );

        let dat = r#"{
            "name": "John",
            "ref_to_name": super[0]
        }"#;
        assert_eq!(
            root(dat),
            Ok((
                "",
                Eson::Dict(
                    None,
                    vec![
                        (
                            Key::new("name", None),
                            EsonSegment::String("John".to_string())
                        ),
                        (
                            Key::new("ref_to_name", None),
                            EsonSegment::Ref(EsonRef::Super(vec![RefIndex::Int(0)]))
                        ),
                    ]
                        .into_iter()
                        .collect(),
                ),
            ))
        );
    }
}
