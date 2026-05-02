use crate::{create_range, Expr, Node, Span};
use ordered_float::OrderedFloat;
use winnow::ascii::{dec_int, float, hex_uint};
use winnow::combinator::{alt, opt, preceded};
use winnow::prelude::*;
use winnow::stream::{Offset, Stream};
use winnow::token::{literal, take_while};

pub fn parse_number<'a>(input: &mut Span<'a>) -> ModalResult<Node> {
    let start = input.checkpoint();

    let sign = opt(alt((literal('+').value(1i64), literal('-').value(-1i64))))
        .parse_next(input)?
        .unwrap_or(1i64);

    let expr = alt((
        parse_hex(sign),
        parse_oct(sign),
        parse_bin(sign),
        literal("Infinity").value(Expr::Float(OrderedFloat(if sign == 1 {
            f64::INFINITY
        } else {
            f64::NEG_INFINITY
        }))),
        literal("NaN").value(Expr::Float(OrderedFloat(f64::NAN))),
        dispatch_dec_or_float(sign),
    ))
    .parse_next(input)?;

    let end = input.checkpoint();
    Ok(Node::new(
        expr,
        create_range(input.offset_from(&start), input.offset_from(&end)),
    ))
}

fn parse_hex<'a>(sign: i64) -> impl FnMut(&mut Span<'a>) -> ModalResult<Expr> {
    move |input: &mut Span<'a>| {
        preceded("0x", hex_uint::<_, u64, _>)
            .map(|v: u64| Expr::Int(v as i64 * sign))
            .parse_next(input)
    }
}

fn parse_oct<'a>(sign: i64) -> impl FnMut(&mut Span<'a>) -> ModalResult<Expr> {
    move |input: &mut Span<'a>| {
        preceded("0o", take_while(1.., |c: char| c.is_digit(8)))
            .try_map(|s: &str| i64::from_str_radix(s, 8).map(|v| v * sign))
            .map(Expr::Int)
            .parse_next(input)
    }
}

fn parse_bin<'a>(sign: i64) -> impl FnMut(&mut Span<'a>) -> ModalResult<Expr> {
    move |input: &mut Span<'a>| {
        preceded("0b", take_while(1.., |c: char| c.is_digit(2)))
            .try_map(|s: &str| i64::from_str_radix(s, 2).map(|v| v * sign))
            .map(Expr::Int)
            .parse_next(input)
    }
}

fn dispatch_dec_or_float<'a>(sign: i64) -> impl FnMut(&mut Span<'a>) -> ModalResult<Expr> {
    move |input: &mut Span<'a>| {
        let checkpoint = input.checkpoint();

        // Peek to see if it's likely a float (contains . or e)
        let s: &str = winnow::token::take_while(1.., ('0'..='9', '.', 'e', 'E', '+', '-'))
            .parse_next(input)?;
        input.reset(&checkpoint);

        if s.contains('.') || s.contains('e') || s.contains('E') {
            let f = float::<_, f64, winnow::error::ContextError>.parse_next(input)?;
            Ok(Expr::Float(OrderedFloat(f * sign as f64)))
        } else {
            let i: i64 = dec_int.parse_next(input)?;
            Ok(Expr::Int(i * sign))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_special_symbol() {
        let mut s = Span::new("Infinity");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), f64::INFINITY),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("-Infinity");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), f64::NEG_INFINITY),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("NaN");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert!(f.into_inner().is_nan()),
            _ => panic!("Expected float"),
        }
    }

    #[test]
    fn test_parse_hex() {
        let mut s = Span::new("0x1af");
        let expr = parse_hex(1)(&mut s).unwrap();
        match expr {
            Expr::Int(v) => assert_eq!(v, 0x1af),
            _ => panic!("Expected int"),
        }
    }

    #[test]
    fn test_parse_oct() {
        let mut s = Span::new("0o123");
        let expr = parse_oct(1)(&mut s).unwrap();
        match expr {
            Expr::Int(v) => assert_eq!(v, 0o123),
            _ => panic!("Expected int"),
        }
    }

    #[test]
    fn test_parse_bin() {
        let mut s = Span::new("0b101");
        let expr = parse_bin(1)(&mut s).unwrap();
        match expr {
            Expr::Int(v) => assert_eq!(v, 0b101),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("0b1001");
        let expr = parse_bin(-1)(&mut s).unwrap();
        match expr {
            Expr::Int(v) => assert_eq!(v, -0b1001),
            _ => panic!("Expected int"),
        }
    }

    #[test]
    fn test_parse_dec() {
        let mut s = Span::new("1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, 1),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("1.0");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 1.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1e1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 10.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1e-1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 0.1),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1.0e+1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 10.0),
            _ => panic!("Expected float"),
        }
    }

    #[test]
    fn test_parse_number_hex() {
        let mut s = Span::new("0x123");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, 291),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("-0x123");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, -291),
            _ => panic!("Expected int"),
        }
    }

    #[test]
    fn test_parse_number_oct() {
        let mut s = Span::new("0o777");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, 511),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("-0o777");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, -511),
            _ => panic!("Expected int"),
        }
    }

    #[test]
    fn test_parse_number_bin() {
        let mut s = Span::new("0b101");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, 5),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("-0b101");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, -5),
            _ => panic!("Expected int"),
        }
    }

    #[test]
    fn test_e() {
        let mut s = Span::new("1.0");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 1.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1.0e1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 10.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1.0e-1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 0.1),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1.0e+1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 10.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1.0E1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 10.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1.0E-1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 0.1),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("1.0E+1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 10.0),
            _ => panic!("Expected float"),
        }
    }

    #[test]
    fn test_more_base() {
        let mut s = Span::new("0b1010");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, 10),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("123.456");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 123.456),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("123.456e-10");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 0.0000000123456),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("123.456e+10");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 1234560000000.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("123.456e10");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), 1234560000000.0),
            _ => panic!("Expected float"),
        }

        let mut s = Span::new("123");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, 123),
            _ => panic!("Expected int"),
        }
    }

    #[test]
    fn test_ordered_float_e_eq() {
        let a = 1.0;
        let b = 1.0e0;
        assert!(a == b);
    }

    #[test]
    fn test_neg() {
        let mut s = Span::new("-0b101");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, -5),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("-0o777");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, -511),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("-0x123");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, -291),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("-1");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Int(v) => assert_eq!(v, -1),
            _ => panic!("Expected int"),
        }

        let mut s = Span::new("-1.0");
        let node = parse_number(&mut s).unwrap();
        match *node.expr {
            Expr::Float(f) => assert_eq!(f.into_inner(), -1.0),
            _ => panic!("Expected float"),
        }
    }
}
