use tokenizer::{Token, TokenRange};
use crate::expr::ExprUnit;

macro_rules! prefix_op {
    ($trait_name:ident, $fn_name:ident, $op:tt) => {
        #[doc(alias = $op)]
        pub trait $trait_name {
            /// The resulting type after applying the unary `$op` operator.
            type Output;

            /// Performs the unary `$op` operation.
            #[must_use = "this returns the result of the operation, without modifying the original"]
            fn $fn_name(self) -> Self::Output;
        }
    };
}

// prefix ops: + - !
prefix_op!(Not, not, "!");
prefix_op!(Neg, neg, "-");
prefix_op!(Pos, pos, "+");

macro_rules! infix_op {
    ($trait_name:ident, $fn_name:ident, $op:tt) => {
        #[doc(alias = $op)]
        pub trait $trait_name<Rhs = Self> {
            /// The resulting type after applying the `$op` operator.
            type Output;

            /// Performs the `$op` operation.
            #[must_use = "this returns the result of the operation, without modifying the original"]
            fn $fn_name(self, rhs: Rhs) -> Self::Output;
        }
    };
}

// infix ops: + - * / % == != < <= > >= && || |
infix_op!(Add, add, "+");
infix_op!(Sub, sub, "-");
infix_op!(Mul, mul, "*");
infix_op!(Div, div, "/");
infix_op!(Mod, mo_, "%");
infix_op!(Eq, eq_, "==");
infix_op!(Ne, ne_, "!=");
infix_op!(Lt, lt_, "<");
infix_op!(Le, le_, "<=");
infix_op!(Gt, gt_, ">");
infix_op!(Ge, ge_, ">=");
infix_op!(And, and, "&&");
infix_op!(Or, or, "||");
infix_op!(Pipe, pipe, "|");

macro_rules! postfix_op {
    ($trait_name:ident, $fn_name:ident, $op:tt) => {
        #[doc(alias = $op)]
        pub trait $trait_name {
            /// The resulting type after applying the unary `$op` operator.
            type Output;

            /// Performs the unary `$op` operation.
            #[must_use = "this returns the result of the operation, without modifying the original"]
            fn $fn_name(self) -> Self::Output;
        }
    };
}

// postfix ops:

macro_rules! ternary_op {
    ($trait_name:ident, $fn_name:ident, $op:tt) => {
        #[doc(alias = $op)]
        pub trait $trait_name<Rhs = Self> {
            /// The resulting type after applying the `$op` operator.
            type Output;

            /// Performs the `$op` operation.
            #[must_use = "this returns the result of the operation, without modifying the original"]
            fn $fn_name(self, rhs1: Rhs, rhs2: Rhs) -> Self::Output;
        }
    };
}

// ternary ops: expr? expr1 : expr2
ternary_op!(Ternary, ternary, "?");

// impl prefix ops: + - !
impl Not for ExprUnit {
    type Output = Self;

    fn not(self) -> Self::Output {
        match self {
            ExprUnit::UnitPrimBoolean(b, tr) => ExprUnit::UnitPrimBoolean(!b, tr),
            _ => unimplemented!(),
        }
    }
}

impl Neg for ExprUnit {
    type Output = Self;

    fn neg(self) -> Self::Output {
        match self {
            ExprUnit::UnitPrimNumberInt(n, _) => ExprUnit::UnitPrimNumberInt(-n, TokenRange::default()),
            ExprUnit::UnitPrimNumberFloat(n,_) => ExprUnit::UnitPrimNumberFloat(-n, TokenRange::default()),
            _ => unimplemented!(),
        }
    }
}

impl Pos for ExprUnit {
    type Output = Self;

    fn pos(self) -> Self::Output {
        self
    }
}

// impl infix ops: + - * / % == != < <= > >= && || |
impl Add for ExprUnit {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        match (self.clone(), rhs.clone()) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimNumberInt(n1 + n2, TokenRange::default())
            }
            (ExprUnit::UnitPrimNumberFloat(n1, _), ExprUnit::UnitPrimNumberFloat(n2, _)) => {
                ExprUnit::UnitPrimNumberFloat(n1 + n2, TokenRange::default())
            }
            // (Unit::UnitPrimNumberInt(n1), Unit::UnitRef(n2)) => {
            //     Unit::UnitInCompleteSemi(TokenChunk::from(vec![
            //         Token::TokenPrimNumber(n1),
            //         Token::TokenOpAdd,
            //         Token::TokenFrameRef(n2),
            //     ]))
            // }
            (left, right) => {
                unimplemented!("left: {:?}, right: {:?}", left, right)
            }
        }
    }
}

impl Sub for ExprUnit {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimNumberInt(n1 - n2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Mul for ExprUnit {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimNumberInt(n1 * n2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Div for ExprUnit {
    type Output = Self;

    fn div(self, rhs: Self) -> Self::Output {
        fn rtn(r: f64) -> ExprUnit {
            if r == r.floor() {
                return ExprUnit::UnitPrimNumberInt(r as i64, TokenRange::default());
            }
            return ExprUnit::UnitPrimNumberFloat(r, TokenRange::default());
        }
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(i, _), ExprUnit::UnitPrimNumberInt(j, _)) => {
                rtn(i as f64 / j as f64)
            }
            (ExprUnit::UnitPrimNumberInt(i, _), ExprUnit::UnitPrimNumberFloat(j, _)) => rtn(i as f64 / j),
            (ExprUnit::UnitPrimNumberFloat(i, _), ExprUnit::UnitPrimNumberInt(j, _)) => rtn(i / j as f64),
            (ExprUnit::UnitPrimNumberFloat(i, _), ExprUnit::UnitPrimNumberFloat(j, _)) => rtn(i / j),
            _ => unimplemented!(),
        }
    }
}

impl Mod for ExprUnit {
    type Output = Self;

    fn mo_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimNumberInt(n1 % n2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Eq for ExprUnit {
    type Output = Self;

    fn eq_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimBoolean(n1 == n2, TokenRange::default())
            }
            (ExprUnit::UnitPrimString(s1, _), ExprUnit::UnitPrimString(s2,  _)) => {
                ExprUnit::UnitPrimBoolean(s1 == s2, TokenRange::default())
            }
            (ExprUnit::UnitPrimBoolean(b1, _), ExprUnit::UnitPrimBoolean(b2, _)) => {
                ExprUnit::UnitPrimBoolean(b1 == b2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Ne for ExprUnit {
    type Output = Self;

    fn ne_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimBoolean(n1 != n2, TokenRange::default())
            }
            (ExprUnit::UnitPrimString(s1, _), ExprUnit::UnitPrimString(s2,  _)) => {
                ExprUnit::UnitPrimBoolean(s1 != s2, TokenRange::default())
            }
            (ExprUnit::UnitPrimBoolean(b1, _), ExprUnit::UnitPrimBoolean(b2, _)) => {
                ExprUnit::UnitPrimBoolean(b1 != b2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Lt for ExprUnit {
    type Output = Self;

    fn lt_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimBoolean(n1 < n2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Le for ExprUnit {
    type Output = Self;

    fn le_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimBoolean(n1 <= n2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Gt for ExprUnit {
    type Output = Self;

    fn gt_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimBoolean(n1 > n2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Ge for ExprUnit {
    type Output = Self;

    fn ge_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimNumberInt(n1, _), ExprUnit::UnitPrimNumberInt(n2, _)) => {
                ExprUnit::UnitPrimBoolean(n1 >= n2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl And for ExprUnit {
    type Output = Self;

    fn and(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimBoolean(b1, _), ExprUnit::UnitPrimBoolean(b2, _)) => {
                ExprUnit::UnitPrimBoolean(b1 && b2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Or for ExprUnit {
    type Output = Self;

    fn or(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (ExprUnit::UnitPrimBoolean(b1, _), ExprUnit::UnitPrimBoolean(b2, _)) => {
                ExprUnit::UnitPrimBoolean(b1 || b2, TokenRange::default())
            }
            _ => unimplemented!(),
        }
    }
}

impl Pipe for ExprUnit {
    type Output = Self;

    fn pipe(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            _ => unimplemented!(),
        }
    }
}

// impl postfix ops:

// impl ternary ops: expr? expr1 : expr2

fn ternary(case: bool, a: ExprUnit, b: ExprUnit) -> ExprUnit {
    if case {
        return a;
    }
    return b;
}

impl Ternary for ExprUnit {
    type Output = Self;

    fn ternary(self, rhs1: Self, rhs2: Self) -> Self::Output {
        match self {
            ExprUnit::UnitPrimNumberInt(n, _) => ternary(n == 0, rhs2, rhs1),
            ExprUnit::UnitPrimNumberFloat(n, _) => ternary(n == 0.0, rhs2, rhs1),
            ExprUnit::UnitPrimString(s, _) => ternary(!s.is_empty(), rhs1, rhs2),
            ExprUnit::UnitPrimBoolean(b, _) => ternary(b, rhs1, rhs2),
            ExprUnit::UnitPrimNull(_) => rhs2,
            ExprUnit::UnitFrameDict(d, _) => ternary(!d.is_empty(), rhs1, rhs2),
            ExprUnit::UnitFrameList(l, _) => ternary(!l.is_empty(), rhs1, rhs2),
            _ => unimplemented!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::EsonNumber;

    #[test]
    fn test_calculate() {
        let a = EsonNumber::from(3);
        let b = EsonNumber::from(4);
        assert_eq!(a / b, EsonNumber::from(0.75));
        assert_eq!(a + b, EsonNumber::from(7));
        assert_eq!(a - b, EsonNumber::from(-1));
        assert_eq!(a * b, EsonNumber::from(12));
        assert_eq!(a % b, EsonNumber::from(3));
    }
}
