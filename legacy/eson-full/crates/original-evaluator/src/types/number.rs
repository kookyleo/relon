use std::fmt::Display;
use std::ops::{Add, Div, Mul, Rem, Sub};

use ordered_float::OrderedFloat;

use tokenizer::Token;

// todo!()
// Instead of using f64 internally, should use a more faithful representation of the original type, and then format the output to determine if it is an integer by using fraction() == 0
#[derive(Debug, Eq, PartialEq, PartialOrd, Copy, Clone)]
pub struct EsonNumber(pub OrderedFloat<f64>);

impl Default for EsonNumber {
    fn default() -> Self {
        EsonNumber(OrderedFloat(0.0))
    }
}

// #[derive(Debug, PartialEq, PartialOrd, Copy, Clone)]
// pub struct EsonNumber(pub(crate) f64);
impl EsonNumber {
    pub fn is_zero(&self) -> bool {
        self.0 == 0.0
    }

    pub fn negative(&self) -> EsonNumber {
        EsonNumber(-self.0)
    }
}

macro_rules! impl_op {
    ($trait:ident, $method:ident) => {
        impl $trait for EsonNumber {
            type Output = Self;

            fn $method(self, rhs: Self) -> Self::Output {
                EsonNumber(self.0.$method(rhs.0))
            }
        }
    };
}

impl_op!(Add, add);
impl_op!(Sub, sub);
impl_op!(Mul, mul);
impl_op!(Div, div);
impl_op!(Rem, rem);

// "=="
// "!="
// "<"
// "<="
// ">"
// ">="

impl From<OrderedFloat<f64>> for EsonNumber {
    fn from(n: OrderedFloat<f64>) -> EsonNumber {
        EsonNumber(n)
    }
}

impl From<i64> for EsonNumber {
    fn from(n: i64) -> EsonNumber {
        EsonNumber(OrderedFloat(n as f64))
    }
}

impl From<f64> for EsonNumber {
    fn from(n: f64) -> EsonNumber {
        EsonNumber(OrderedFloat(n))
    }
}

impl From<EsonNumber> for f64 {
    fn from(n: EsonNumber) -> f64 {
        *n.0
    }
}

impl From<Token> for EsonNumber {
    fn from(t: Token) -> EsonNumber {
        match t {
            Token::TokenPrimNumberInt(n, _) => EsonNumber(OrderedFloat(n as f64)),
            Token::TokenPrimNumberFloat(n, _) => EsonNumber(OrderedFloat(n)),
            _ => panic!("Expected numeric, found {:?}", t),
        }
    }
}

impl Display for EsonNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.fract() == 0.0 {
            write!(f, "{}", self.0 .0 as i64)
        } else {
            write!(f, "{}", self.0)
        }
    }
}
