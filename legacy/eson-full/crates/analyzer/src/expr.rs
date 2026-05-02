use std::fmt::Display;

use example_evaluator::util_iter::Iter;

use crate::expr_token::Token;
use crate::TokenChunk;

#[derive(Debug, PartialEq)]
pub enum Expr {
    Primary(Token),
    PrefixOp(Token, Box<Expr>),
    InfixOp(Token, Box<Expr>, Box<Expr>),
    PostfixOp(Token, Box<Expr>),
    TernaryOp(Box<Expr>, Box<Expr>, Box<Expr>),
}

impl Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Primary(token) => write!(f, "{}", token),
            Expr::PrefixOp(token, rhs) => write!(f, "({}{})", token, rhs),
            Expr::InfixOp(token, lhs, rhs) => write!(f, "({}{}{})", lhs, token, rhs),
            Expr::PostfixOp(token, lhs) => write!(f, "({}{})", lhs, token),
            Expr::TernaryOp(cond, true_expr, false_expr) => {
                write!(f, "({}?{}:{})", cond, true_expr, false_expr)
            }
        }
    }
}

#[derive(Debug)]
pub struct PrattParser<'a>(Iter<'a, Token>);

impl PrattParser<'_> {
    pub fn parse(chunk: &mut TokenChunk) -> Expr {
        let mut parser = PrattParser(Iter::from(
            <&mut TokenChunk as Into<&mut Vec<Token>>>::into(chunk),
        ));
        parser.process(0)
    }

    // operator precedence table
    fn prec(token: &str) -> u8 {
        match token {
            "()" => 90,
            "!" => 80,
            "*" | "/" | "%" => 70,
            "+" | "-" => 65,
            ">" | "<" | "<=" | ">=" => 60,
            "==" | "!=" => 55,
            "&&" => 50,
            "||" => 45,
            "?" => 20,
            ":" => 15,
            "|" => 10,
            _ => 0,
        }
    }

    // (..) group sub expression
    fn group(chunk: &mut TokenChunk) -> Expr {
        // let mut sub_parser = PrattParser::from(chunk);
        // let mut sub_parser = PrattParser::new(chunk);
        // sub_parser.process(0)
        PrattParser::parse(chunk)
    }

    fn take_next(&mut self) -> Option<Token> {
        self.0.take_next()
    }

    fn peek(&self) -> Option<&Token> {
        self.0.peek()
    }

    fn process(&mut self, prec: u8) -> Expr {
        let token = self.take_next().expect("Unexpected EOF");
        let mut lhs = match token {
            Token::Primitive(..) => Expr::Primary(token),
            Token::Group(mut chunk) => Self::group(&mut chunk),
            Token::OpNot => Expr::PrefixOp(token, Box::new(self.process(Self::prec("!")))),
            Token::OpAdd => Expr::PrefixOp(token, Box::new(self.process(Self::prec("+")))),
            Token::OpSub => Expr::PrefixOp(token, Box::new(self.process(Self::prec("-")))),
            _ => panic!("Unexpected prefix token {:?}", &token),
        };
        let mut precedence_r = self
            .peek()
            .map_or(0, |token| Self::prec(format!("{}", token).as_str()));

        while prec < precedence_r {
            let token = self.take_next().unwrap();
            lhs = match token {
                Token::Group(mut chunk) => Self::group(&mut chunk),
                Token::OpOr => Expr::InfixOp(
                    token,
                    Box::new(lhs),
                    Box::new(self.process(Self::prec("||"))),
                ),
                Token::OpAnd => Expr::InfixOp(
                    token,
                    Box::new(lhs),
                    Box::new(self.process(Self::prec("&&"))),
                ),
                Token::OpEq | Token::OpNe => Expr::InfixOp(
                    token,
                    Box::new(lhs),
                    Box::new(self.process(Self::prec("=="))),
                ),
                Token::OpLt | Token::OpGt | Token::OpLe | Token::OpGe => Expr::InfixOp(
                    token,
                    Box::new(lhs),
                    Box::new(self.process(Self::prec("<"))),
                ),
                Token::OpAdd | Token::OpSub => Expr::InfixOp(
                    token,
                    Box::new(lhs),
                    Box::new(self.process(Self::prec("+"))),
                ),
                Token::OpMul | Token::OpDiv | Token::OpMod => Expr::InfixOp(
                    token,
                    Box::new(lhs),
                    Box::new(self.process(Self::prec("*"))),
                ),
                Token::OpNot => Expr::PrefixOp(token, Box::new(self.process(Self::prec("!")))),
                // expr ? expr : expr
                Token::OpQ => {
                    // -1 here makes sure that the ternary operator is right associative
                    let expr_t = self.process(Self::prec("?") - 1);
                    match self.take_next() {
                        Some(Token::OpColon) => Expr::TernaryOp(
                            Box::new(lhs),
                            Box::new(expr_t),
                            Box::new(self.process(prec)),
                        ),
                        Some(token) => panic!("Expected ':' in ternary expr, found {:?}", token),
                        None => panic!("Unexpected EOF in ternary expr"),
                    }
                }
                Token::OpPipe => Expr::InfixOp(
                    token,
                    Box::new(lhs),
                    Box::new(self.process(Self::prec("|"))),
                ),
                _ => panic!("Unexpected expr-chunk token {:?}", token),
            };
            precedence_r = self
                .peek()
                .map_or(0, |token| Self::prec(format!("{}", token).as_str()));
        }
        lhs
    }
}

#[cfg(test)]
mod tests {
    use crate::expr_token::expr;
    use crate::EsonSegment;

    use super::*;

    #[test]
    fn test_simple_test() {
        let (_remaining, mut token_chunk) = expr(r#"1 + 1"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);

        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
            )
        );
    }

    #[test]
    fn test_pipe() {
        let (_remaining, mut token_chunk) = expr(r#"1 | 2"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpPipe,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"1 | 2 | 3"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpPipe,
                Box::new(Expr::InfixOp(
                    Token::OpPipe,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"1 | fn(1)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpPipe,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                    "fn".to_string(),
                    vec![EsonSegment::Int(1)],
                )))),
            )
        );
    }

    #[test]
    fn test_ternary() {
        let (_remaining, mut token_chunk) = expr(r#"true ? 1 : 2"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"true ? 1 : 2 + 3"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::InfixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
                )),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"true ? 1 + 2 : 3 + 4"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                Box::new(Expr::InfixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                )),
                Box::new(Expr::InfixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                )),
            )
        );

        // ("4 * 5 ? 6 : 7"),
        let (_remaining, mut token_chunk) = expr(r#"4 * 5 ? 6 : 7"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::InfixOp(
                    Token::OpMul,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(5)))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(6)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(7)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"1 ? 2 : (4 ? 8 : 16)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                Box::new(Expr::TernaryOp(
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(8)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(16)))),
                )),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"1 ? 2 : 4 ? 8: 16"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                Box::new(Expr::TernaryOp(
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(8)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(16)))),
                )),
            )
        );

        // ("1 ? 2 : 4 ? 8 + 16 : 32 * 64")
        let (_remaining, mut token_chunk) = expr(r#"1 ? 2 : 4 ? 8 + 16 : 32 * 64"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                Box::new(Expr::TernaryOp(
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                    Box::new(Expr::InfixOp(
                        Token::OpAdd,
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(8)))),
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(16)))),
                    )),
                    Box::new(Expr::InfixOp(
                        Token::OpMul,
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(32)))),
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(64)))),
                    )),
                )),
            )
        );

        // ("1 ? 2 + 3 : 4 * 5"),
        let (_remaining, mut token_chunk) = expr(r#"1 ? 2 + 3 : 4 * 5"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::InfixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
                )),
                Box::new(Expr::InfixOp(
                    Token::OpMul,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(5)))),
                )),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"(1 + 2 * 3)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::InfixOp(
                    Token::OpMul,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
                )),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"(true? 1: 2)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::TernaryOp(
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"(true? 1: 2) + 3"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::TernaryOp(
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
            )
        );
    }

    #[test]
    fn test_not_and_or() {
        let (_remaining, mut token_chunk) = expr(r#"!true"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::PrefixOp(
                Token::OpNot,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"!true && false"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAnd,
                Box::new(Expr::PrefixOp(
                    Token::OpNot,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(false)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"!true && !false"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAnd,
                Box::new(Expr::PrefixOp(
                    Token::OpNot,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                )),
                Box::new(Expr::PrefixOp(
                    Token::OpNot,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(false)))),
                )),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"true || false && !true"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpOr,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                Box::new(Expr::InfixOp(
                    Token::OpAnd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(false)))),
                    Box::new(Expr::PrefixOp(
                        Token::OpNot,
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Boolean(true)))),
                    )),
                )),
            )
        );
    }

    #[test]
    fn test_positive() {
        let (_remaining, mut token_chunk) = expr(r#"+1"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::PrefixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"+1 + 2"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::PrefixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"+1 + +2"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::PrefixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                )),
                Box::new(Expr::PrefixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                )),
            )
        );
    }

    #[test]
    fn test_negative() {
        let (_remaining, mut token_chunk) = expr(r#"-1"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::PrefixOp(
                Token::OpSub,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"-1 + 2"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::PrefixOp(
                    Token::OpSub,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"-1 + -2"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::PrefixOp(
                    Token::OpSub,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                )),
                Box::new(Expr::PrefixOp(
                    Token::OpSub,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                )),
            )
        );
    }

    #[test]
    fn test_mix() {
        // A + B < C && D * E > F
        let (_remaining, mut token_chunk) = expr(r#"A + B < C && D * E > F"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAnd,
                Box::new(Expr::InfixOp(
                    Token::OpLt,
                    Box::new(Expr::InfixOp(
                        Token::OpAdd,
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                            "A".to_string()
                        )))),
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                            "B".to_string()
                        )))),
                    )),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                        "C".to_string()
                    )))),
                )),
                Box::new(Expr::InfixOp(
                    Token::OpGt,
                    Box::new(Expr::InfixOp(
                        Token::OpMul,
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                            "D".to_string()
                        )))),
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                            "E".to_string()
                        )))),
                    )),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                        "F".to_string()
                    )))),
                )),
            )
        );
    }

    #[test]
    fn test_expr() {
        let (_remaining, mut token_chunk) = expr(r#"1 + 2 * 3"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::InfixOp(
                    Token::OpMul,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
                )),
            )
        );
    }

    #[test]
    fn test_expr_group() {
        let (_remaining, mut token_chunk) = expr(r#"1 + (2 + 3) * 4"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::InfixOp(
                    Token::OpMul,
                    Box::new(Expr::InfixOp(
                        Token::OpAdd,
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
                    )),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                )),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"(1 + (4 - 2)) * 3"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpMul,
                Box::new(Expr::InfixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                    Box::new(Expr::InfixOp(
                        Token::OpSub,
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                        Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                    )),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(3)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"(1 + 2 * 4)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(1)))),
                Box::new(Expr::InfixOp(
                    Token::OpMul,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(4)))),
                )),
            )
        );
    }

    #[test]
    fn test_fn_call() {
        let (_remaining, mut token_chunk) = expr(r#"f(1, 2)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                "f".to_string(),
                vec![EsonSegment::Int(1), EsonSegment::Int(2)],
            )))
        );

        let (_remaining, mut token_chunk) = expr(r#"f(1, 2) + f(3, 4)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                    "f".to_string(),
                    vec![EsonSegment::Int(1), EsonSegment::Int(2)],
                )))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                    "f".to_string(),
                    vec![EsonSegment::Int(3), EsonSegment::Int(4)],
                )))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"f(1, f(3, 4))"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                "f".to_string(),
                vec![
                    EsonSegment::Int(1),
                    EsonSegment::FnCall(
                        "f".to_string(),
                        vec![EsonSegment::Int(3), EsonSegment::Int(4)],
                    ),
                ],
            )))
        );

        let (_remaining, mut token_chunk) = expr(r#"f(1, f(3, 4)) + 2"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                    "f".to_string(),
                    vec![
                        EsonSegment::Int(1),
                        EsonSegment::FnCall(
                            "f".to_string(),
                            vec![EsonSegment::Int(3), EsonSegment::Int(4)],
                        ),
                    ],
                )))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(2)))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"(f(1, f(3, 4)) + 7) * f(5, 6)"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpMul,
                Box::new(Expr::InfixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                        "f".to_string(),
                        vec![
                            EsonSegment::Int(1),
                            EsonSegment::FnCall(
                                "f".to_string(),
                                vec![EsonSegment::Int(3), EsonSegment::Int(4)],
                            ),
                        ],
                    )))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Int(7)))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::FnCall(
                    "f".to_string(),
                    vec![EsonSegment::Int(5), EsonSegment::Int(6)],
                )))),
            )
        );
    }

    #[test]
    fn test_var() {
        let (_remaining, mut token_chunk) = expr(r#"a + b"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpAdd,
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                    "a".to_string()
                )))),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                    "b".to_string()
                )))),
            )
        );

        let (_remaining, mut token_chunk) = expr(r#"(a + b) * c"#).unwrap();
        let r = PrattParser::parse(&mut token_chunk);
        assert_eq!(
            r,
            Expr::InfixOp(
                Token::OpMul,
                Box::new(Expr::InfixOp(
                    Token::OpAdd,
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                        "a".to_string()
                    )))),
                    Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                        "b".to_string()
                    )))),
                )),
                Box::new(Expr::Primary(Token::Primitive(EsonSegment::Var(
                    "c".to_string()
                )))),
            )
        );
    }
}
