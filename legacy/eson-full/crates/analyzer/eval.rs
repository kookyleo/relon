use std::cell::RefMut;

use parser::{EsonSegment, Expr, PrattParser, Token, TokenChunk};

use crate::compute::Compute;
use crate::context::Context;
use crate::ops::*;

pub trait Eval {
    fn eval(self, ctx: RefMut<Context>) -> EsonSegment;
}

impl Eval for &mut TokenChunk {
    fn eval(self, ctx: RefMut<Context>) -> EsonSegment {
        let expr = PrattParser::parse(self);
        expr.eval(ctx)
    }
}

impl Eval for Expr {
    fn eval(self, ctx: RefMut<Context>) -> EsonSegment {
        match self {
            Expr::Primary(t) => t.eval(ctx),
            Expr::PrefixOp(t, expr) => {
                // ops: + - !
                match t {
                    Token::OpAdd => expr.eval(ctx).pos(), // positive +
                    Token::OpSub => expr.eval(ctx).neg(), // negative -
                    Token::OpNot => expr.eval(ctx).not(), // not !
                    _ => unreachable!("unexpected case"),
                }
            }
            Expr::InfixOp(t, expr1, expr2) => {
                // ops: + - * / % == != < <= > >= && || |
                match t {
                    Token::OpAdd => expr1.eval(ctx).add(expr2.eval(ctx)), // add +
                    Token::OpSub => expr1.eval(ctx).sub(expr2.eval(ctx)), // sub -
                    Token::OpMul => expr1.eval(ctx).mul(expr2.eval(ctx)), // multiply *
                    Token::OpDiv => expr1.eval(ctx).div(expr2.eval(ctx)), // divide /
                    Token::OpMod => expr1.eval(ctx).mo_(expr2.eval(ctx)), // mod %
                    Token::OpEq => expr1.eval(ctx).eq_(expr2.eval(ctx)),  // equal ==
                    Token::OpNe => expr1.eval(ctx).ne_(expr2.eval(ctx)),  // not equal !=
                    Token::OpLt => expr1.eval(ctx).lt_(expr2.eval(ctx)),  // less than <
                    Token::OpLe => expr1.eval(ctx).le_(expr2.eval(ctx)),  // less equal <=
                    Token::OpGt => expr1.eval(ctx).gt_(expr2.eval(ctx)),  // greater than >
                    Token::OpGe => expr1.eval(ctx).ge_(expr2.eval(ctx)),  // greater equal >=
                    Token::OpAnd => expr1.eval(ctx).and(expr2.eval(ctx)), // and &&
                    Token::OpOr => expr1.eval(ctx).or(expr2.eval(ctx)),   // or ||
                    Token::OpPipe => expr1.eval(ctx).pipe(expr2.eval(ctx)), // pipe |
                    _ => unreachable!("unexpected case"),
                }
            }
            Expr::PostfixOp(t, expr) => {
                // ops:
                match t {
                    _ => unreachable!("unexpected case"),
                }
            }
            Expr::TernaryOp(expr, expr1, expr2) => {
                expr.eval(ctx).ternary(expr1.eval(ctx), expr2.eval(ctx))
            }
        }
    }
}

impl Eval for Token {
    fn eval(mut self, ctx: RefMut<Context>) -> EsonSegment {
        match self {
            Token::Group(ref mut tc) => tc.eval(ctx),
            Token::Primitive(mut ev) => {
                ev.compute(ctx);
                ev.val()
            }
            _ => unreachable!("unexpected escape"),
        }
    }
}

#[cfg(test)]
mod tests {
    use parser::{EsonRef, EsonSegment, Key, PrattParser, RefIndex, Token, TokenChunk};

    use crate::context::Context;
    use crate::eval::Eval;

    #[test]
    fn test_simple() {
        let mut tc: TokenChunk = TokenChunk::from(vec![
            Token::Primitive(EsonSegment::Int(1)),
            Token::OpAdd,
            Token::Primitive(EsonSegment::Int(1)),
        ]);

        let expr = PrattParser::parse(&mut tc);
        let mut ctx = Context::new();
        let r = expr.eval(&mut ctx);
        assert_eq!(r, EsonSegment::Int(2));
    }

    #[test]
    fn test_var() {
        let mut ctx = Context::new();
        ctx.set_variable("a", EsonSegment::Int(1));

        let mut tc: TokenChunk = TokenChunk::from(vec![
            Token::Primitive(EsonSegment::Int(1)),
            Token::OpAdd,
            Token::Primitive(EsonSegment::Var("a".to_string())),
        ]);

        let expr = PrattParser::parse(&mut tc);
        let r = expr.eval(&mut ctx);
        assert_eq!(r, EsonSegment::Int(2));
    }
}
