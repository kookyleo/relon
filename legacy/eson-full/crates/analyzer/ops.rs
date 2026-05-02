use parser::EsonSegment;

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

impl Pos for EsonSegment {
    type Output = EsonSegment;

    fn pos(self) -> Self::Output {
        match self {
            EsonSegment::Int(i) => EsonSegment::Int(i),
            EsonSegment::Float(f) => EsonSegment::Float(f),
            _ => unreachable!(),
        }
    }
}

impl Neg for EsonSegment {
    type Output = EsonSegment;

    fn neg(self) -> Self::Output {
        match self {
            EsonSegment::Int(i) => EsonSegment::Int(-i),
            EsonSegment::Float(f) => EsonSegment::Float(-f),
            _ => unreachable!(),
        }
    }
}

impl Not for EsonSegment {
    type Output = EsonSegment;

    fn not(self) -> Self::Output {
        match self {
            EsonSegment::Boolean(b) => EsonSegment::Boolean(!b),
            _ => unreachable!(),
        }
    }
}

impl Add for EsonSegment {
    type Output = EsonSegment;

    fn add(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Int(i1 + i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Float((i1 as f64) + f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Float(f1 + (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Float(f1 + f2),
            _ => unreachable!(),
        }
    }
}

impl Sub for EsonSegment {
    type Output = EsonSegment;

    fn sub(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Int(i1 - i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Float((i1 as f64) - f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Float(f1 - (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Float(f1 - f2),
            _ => unreachable!(),
        }
    }
}

impl Mul for EsonSegment {
    type Output = EsonSegment;

    fn mul(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Int(i1 * i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Float((i1 as f64) * f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Float(f1 * (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Float(f1 * f2),
            _ => unreachable!(),
        }
    }
}

impl Div for EsonSegment {
    type Output = EsonSegment;

    fn div(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Int(i1 / i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Float((i1 as f64) / f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Float(f1 / (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Float(f1 / f2),
            _ => unreachable!(),
        }
    }
}

impl Mod for EsonSegment {
    type Output = EsonSegment;

    fn mo_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Int(i1 % i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Float((i1 as f64) % f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Float(f1 % (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Float(f1 % f2),
            _ => unreachable!(),
        }
    }
}

impl Eq for EsonSegment {
    type Output = EsonSegment;

    fn eq_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Boolean(i1 == i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Boolean((i1 as f64) == f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Boolean(f1 == (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Boolean(f1 == f2),
            (EsonSegment::Boolean(b1), EsonSegment::Boolean(b2)) => EsonSegment::Boolean(b1 == b2),
            (EsonSegment::String(s1), EsonSegment::String(s2)) => EsonSegment::Boolean(s1 == s2),
            _ => unreachable!(),
        }
    }
}

impl Ne for EsonSegment {
    type Output = EsonSegment;

    fn ne_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Boolean(i1 != i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Boolean((i1 as f64) != f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Boolean(f1 != (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Boolean(f1 != f2),
            (EsonSegment::Boolean(b1), EsonSegment::Boolean(b2)) => EsonSegment::Boolean(b1 != b2),
            (EsonSegment::String(s1), EsonSegment::String(s2)) => EsonSegment::Boolean(s1 != s2),
            _ => unreachable!(),
        }
    }
}

impl Lt for EsonSegment {
    type Output = EsonSegment;

    fn lt_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Boolean(i1 < i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Boolean((i1 as f64) < f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Boolean(f1 < (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Boolean(f1 < f2),
            (EsonSegment::String(s1), EsonSegment::String(s2)) => EsonSegment::Boolean(s1 < s2),
            _ => unreachable!(),
        }
    }
}

impl Le for EsonSegment {
    type Output = EsonSegment;

    fn le_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Boolean(i1 <= i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Boolean((i1 as f64) <= f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Boolean(f1 <= (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Boolean(f1 <= f2),
            (EsonSegment::String(s1), EsonSegment::String(s2)) => EsonSegment::Boolean(s1 <= s2),
            _ => unreachable!(),
        }
    }
}

impl Gt for EsonSegment {
    type Output = EsonSegment;

    fn gt_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Boolean(i1 > i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Boolean((i1 as f64) > f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Boolean(f1 > (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Boolean(f1 > f2),
            (EsonSegment::String(s1), EsonSegment::String(s2)) => EsonSegment::Boolean(s1 > s2),
            _ => unreachable!(),
        }
    }
}

impl Ge for EsonSegment {
    type Output = EsonSegment;

    fn ge_(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Int(i1), EsonSegment::Int(i2)) => EsonSegment::Boolean(i1 >= i2),
            (EsonSegment::Int(i1), EsonSegment::Float(f2)) => EsonSegment::Boolean((i1 as f64) >= f2),
            (EsonSegment::Float(f1), EsonSegment::Int(i2)) => EsonSegment::Boolean(f1 >= (i2 as f64)),
            (EsonSegment::Float(f1), EsonSegment::Float(f2)) => EsonSegment::Boolean(f1 >= f2),
            (EsonSegment::String(s1), EsonSegment::String(s2)) => EsonSegment::Boolean(s1 >= s2),
            _ => unreachable!(),
        }
    }
}

impl And for EsonSegment {
    type Output = EsonSegment;

    fn and(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Boolean(b1), EsonSegment::Boolean(b2)) => EsonSegment::Boolean(b1 && b2),
            _ => unreachable!(),
        }
    }
}

impl Or for EsonSegment {
    type Output = EsonSegment;

    fn or(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            (EsonSegment::Boolean(b1), EsonSegment::Boolean(b2)) => EsonSegment::Boolean(b1 || b2),
            _ => unreachable!(),
        }
    }
}

impl Pipe for EsonSegment {
    type Output = EsonSegment;

    fn pipe(self, rhs: Self) -> Self::Output {
        match (self, rhs) {
            _ => unreachable!(),
        }
    }
}

impl Ternary for EsonSegment {
    type Output = EsonSegment;

    fn ternary(self, rhs1: Self, rhs2: Self) -> Self::Output {
        match self {
            EsonSegment::Boolean(b) => {
                if b {
                    rhs1
                } else {
                    rhs2
                }
            }
            _ => unreachable!(),
        }
    }
}