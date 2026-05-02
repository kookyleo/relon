use nom::branch::alt;
use nom::bytes::complete::{tag, tag_no_case, take_while1};
use nom::character::complete::{char as ch, digit1};
use nom::combinator::{map, opt};
use nom::error::VerboseError;
use nom::sequence::{preceded, tuple};

use crate::token::Token;

#[derive(Debug, PartialEq)]
pub enum EsonNumeric {
    Int(i64),
    Float(f64),
}

impl From<Token> for EsonNumeric {
    fn from(t: Token) -> EsonNumeric {
        match t {
            Token::PrimInt(i) => EsonNumeric::Int(i),
            Token::PrimFloat(f) => EsonNumeric::Float(f),
            _ => panic!("Expected numeric, found {:?}", t),
        }
    }
}

impl From<EsonNumeric> for i64 {
    fn from(n: EsonNumeric) -> i64 {
        match n {
            EsonNumeric::Int(i) => i,
            _ => panic!("Expected integer, found {:?}", n),
        }
    }
}

impl From<EsonNumeric> for f64 {
    fn from(n: EsonNumeric) -> f64 {
        match n {
            EsonNumeric::Float(f) => f,
            _ => panic!("Expected float, found {:?}", n),
        }
    }
}

impl From<i64> for EsonNumeric {
    fn from(i: i64) -> EsonNumeric {
        EsonNumeric::Int(i)
    }
}

impl From<f64> for EsonNumeric {
    fn from(f: f64) -> EsonNumeric {
        EsonNumeric::Float(f)
    }
}

fn parse_hex(input: &str) -> nom::IResult<&str, &str, VerboseError<&str>> {
    let is_hex_digit = |c: char| c.is_digit(16);
    let (remaining, _) = tag("0x")(input)?;
    take_while1(is_hex_digit)(remaining)
}

fn parse_oct(input: &str) -> nom::IResult<&str, &str, VerboseError<&str>> {
    let is_oct_digit = |c: char| c.is_digit(8);
    let (remaining, _) = tag("0o")(input)?;
    take_while1(is_oct_digit)(remaining)
}

fn parse_bin(input: &str) -> nom::IResult<&str, &str, VerboseError<&str>> {
    let is_bin_digit = |c: char| c.is_digit(2);
    let (remaining, _) = tag("0b")(input)?;
    take_while1(is_bin_digit)(remaining)
}

pub(crate) fn parse_numeric(input: &str) -> nom::IResult<&str, EsonNumeric, VerboseError<&str>> {
    alt((
        map(parse_bin, |s| {
            EsonNumeric::Int(i64::from_str_radix(s, 2).expect("TODO"))
        }),
        map(parse_oct, |s| {
            EsonNumeric::Int(i64::from_str_radix(s, 8).expect("TODO"))
        }),
        map(parse_hex, |s| {
            EsonNumeric::Int(i64::from_str_radix(s, 16).expect("TODO"))
        }),
        map(
            tuple((
                digit1,
                opt(preceded(ch('.'), digit1)),
                opt(preceded(
                    tag_no_case("e"),
                    tuple((opt(alt((ch('+'), ch('-')))), digit1)),
                )),
            )),
            |(int_part, decimal_part, exp_part): (
                &str,
                Option<&str>,
                Option<(Option<char>, &str)>,
            )| {
                if decimal_part.is_none() && exp_part.is_none() {
                    // no decimal point or exponent => integer
                    // int_part.parse::<i64>().map(JsonValue::Int)
                    EsonNumeric::Int(int_part.parse::<i64>().expect("TODO"))
                } else {
                    let num_str = format!(
                        "{}{}{}",
                        int_part,
                        decimal_part.map_or(String::from(""), |d| format!(".{}", d)),
                        exp_part.map_or(String::from(""), |(sign, e)| format!(
                            "e{}{}",
                            sign.unwrap_or('+'),
                            e
                        ))
                    );
                    // dbg!(num_str.clone());
                    // num_str.parse::<f64>().map(JsonValue::Float)
                    EsonNumeric::Float(num_str.parse::<f64>().expect("TODO"))
                }
            },
        ),
        map(tag("Infinity"), |_| EsonNumeric::Float(f64::INFINITY)),
        map(tag("-Infinity"), |_| EsonNumeric::Float(f64::NEG_INFINITY)),
        map(tag("NaN"), |_| EsonNumeric::Float(f64::NAN)),
    ))(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_int_transform() {
        let ei = EsonNumeric::Int(42);
        let i: i64 = ei.into();
        assert_eq!(i, 42);

        let ei = EsonNumeric::Int(42);
        let token = Token::PrimInt(42);
        assert_eq!(ei, token.into());

        let i: i64 = 42;
        let ei = EsonNumeric::Int(42);
        assert_eq!(i, ei.into());

        fn test_add(a: i64, b: i64) -> i64 {
            a + b
        }
        let a = EsonNumeric::Int(1);
        let b = EsonNumeric::Int(2);
        let c = test_add(a.into(), b.into());
        assert_eq!(c, 3);
    }

    #[test]
    fn test_float_transform() {
        let ef = EsonNumeric::Float(42.0);
        let f: f64 = ef.into();
        assert_eq!(f, 42.0);

        let ef = EsonNumeric::Float(42.0);
        let token = Token::PrimFloat(42.0);
        assert_eq!(ef, token.into());

        let f: f64 = 42.0;
        let ef = EsonNumeric::Float(42.0);
        assert_eq!(f, ef.into());

        fn test_add(a: f64, b: f64) -> f64 {
            a + b
        }
        let a = EsonNumeric::Float(1.0);
        let b = EsonNumeric::Float(2.0);
        let c = test_add(a.into(), b.into());
        assert_eq!(c, 3.0);
    }

    #[test]
    fn test_e() {
        assert_eq!(parse_numeric("1.0"), Ok(("", EsonNumeric::Float(1.0))));
        assert_eq!(parse_numeric("1.0e1"), Ok(("", EsonNumeric::Float(10.0))));
        assert_eq!(parse_numeric("1.0e-1"), Ok(("", EsonNumeric::Float(0.1))));
        assert_eq!(parse_numeric("1.0e+1"), Ok(("", EsonNumeric::Float(10.0))));
        assert_eq!(parse_numeric("1.0E1"), Ok(("", EsonNumeric::Float(10.0))));
        assert_eq!(parse_numeric("1.0E-1"), Ok(("", EsonNumeric::Float(0.1))));
        assert_eq!(parse_numeric("1.0E+1"), Ok(("", EsonNumeric::Float(10.0))));
    }

    #[test]
    fn test_more_base() {
        assert_eq!(parse_numeric("0b1010"), Ok(("", EsonNumeric::Int(10))));
        assert_eq!(parse_numeric("0o777"), Ok(("", EsonNumeric::Int(511))));
        assert_eq!(parse_numeric("0x123"), Ok(("", EsonNumeric::Int(291))));
        assert_eq!(
            parse_numeric("123.456"),
            Ok(("", EsonNumeric::Float(123.456)))
        );
        assert_eq!(
            parse_numeric("123.456e-10"),
            Ok(("", EsonNumeric::Float(0.0000000123456)))
        );
        assert_eq!(
            parse_numeric("123.456e+10"),
            Ok(("", EsonNumeric::Float(1234560000000.0)))
        );
        assert_eq!(
            parse_numeric("123.456e10"),
            Ok(("", EsonNumeric::Float(1234560000000.0)))
        );
        assert_eq!(parse_numeric("123"), Ok(("", EsonNumeric::Int(123))));
    }
}
