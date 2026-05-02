use std::fmt::Display;
use tokenizer::{ReadTokenRange, Token, TokenKey, TokenRange};

use crate::decorator::decorators_call;
use crate::util_iter::Iter;

#[derive(Debug, PartialEq, Clone)]
pub(crate) enum RefType {
    RefSibling,
    RefUncle,
    RefRoot,
}

#[derive(Debug, PartialEq, Clone)]
pub enum ExprUnit {
    UnitPrimNumberInt(i64, TokenRange),
    UnitPrimNumberFloat(f64, TokenRange),
    UnitPrimString(String, TokenRange),
    UnitPrimBoolean(bool, TokenRange),
    UnitPrimNull(TokenRange),
    UnitFrameRoot(Box<ExprUnit>, TokenRange),
    UnitFrameList(Vec<(TokenKey, ExprUnit)>, TokenRange),
    UnitFrameDict(Vec<(TokenKey, ExprUnit)>, TokenRange),
    UnitPendedRef(RefType, Vec<TokenKey>, TokenRange),
    UnitPendedVar(String, TokenRange),
    UnitPendedFnCall(String, Vec<ExprUnit>, TokenRange),
    UnitPendedExpr(Box<Expr>, TokenRange),
}

impl ExprUnit {
    pub(crate) fn type_name(&self) -> String {
        match self {
            ExprUnit::UnitPrimNumberInt(..) => "UnitPrimNumberInt".to_string(),
            ExprUnit::UnitPrimNumberFloat(..) => "UnitPrimNumberFloat".to_string(),
            ExprUnit::UnitPrimString(..) => "UnitPrimString".to_string(),
            ExprUnit::UnitPrimBoolean(..) => "UnitPrimBoolean".to_string(),
            ExprUnit::UnitPrimNull(..) => "UnitPrimNull".to_string(),
            ExprUnit::UnitFrameRoot(..) => "UnitFrameRoot".to_string(),
            ExprUnit::UnitFrameList(..) => "UnitFrameList".to_string(),
            ExprUnit::UnitFrameDict(..) => "UnitFrameDict".to_string(),
            ExprUnit::UnitPendedRef(..) => "UnitPendedRef".to_string(),
            ExprUnit::UnitPendedVar(..) => "UnitPendedVar".to_string(),
            ExprUnit::UnitPendedFnCall(..) => "UnitPendedFnCall".to_string(),
            ExprUnit::UnitPendedExpr(..) => "UnitPendedExpr".to_string(),
        }
    }
}

impl From<Token> for ExprUnit {
    fn from(token: Token) -> ExprUnit {
        match token {
            Token::TokenPrimNumberInt(i, tr) => ExprUnit::UnitPrimNumberInt(i, tr),
            Token::TokenPrimNumberFloat(f, tr) => ExprUnit::UnitPrimNumberFloat(f, tr),
            Token::TokenPrimString(s, tr) => ExprUnit::UnitPrimString(s, tr),
            Token::TokenPrimBoolean(b, tr) => ExprUnit::UnitPrimBoolean(b, tr),
            Token::TokenPrimNull(tr) => ExprUnit::UnitPrimNull(tr),
            Token::TokenFnCall(n, args, tr) => ExprUnit::UnitPendedFnCall(
                n.to_string(),
                args.into_iter().map(|arg| arg.into()).collect(),
                tr,
            ),
            Token::TokenVar(v, tr) => ExprUnit::UnitPendedVar(v, tr),
            Token::TokenRefVarSibling(r, tr) => ExprUnit::UnitPendedRef(RefType::RefSibling, r, tr),
            Token::TokenRefVarUncle(r, tr) => ExprUnit::UnitPendedRef(RefType::RefUncle, r, tr),
            Token::TokenRefVarRoot(r, tr) => ExprUnit::UnitPendedRef(RefType::RefRoot, r, tr),
            Token::TokenFrameRoot(d, v, tr) => {
                ExprUnit::UnitFrameRoot(Box::from(decorators_call(d, (*v.clone()).into())), tr)
            }
            Token::TokenFrameDict(d, tr) => ExprUnit::UnitFrameDict(
                d.into_iter()
                    .map(|(i, d, v)| (i, decorators_call(d, v.into())))
                    .collect(),
                tr,
            ),
            Token::TokenFrameList(l, tr) => {
                ExprUnit::UnitFrameList(l.into_iter().map(|(i, v)| (i, v.into())).collect(), tr)
            }
            Token::TokenExprSequence(seq, tr) => {
                ExprUnit::UnitPendedExpr(Box::new(PrattParser::parse(&mut TokenChunk(seq))), tr)
            }
            _ => unimplemented!("Unimplemented token {:?}", token),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Operator {
    OpPipe(TokenRange),
    OpEq(TokenRange),
    OpNe(TokenRange),
    OpLe(TokenRange),
    OpGe(TokenRange),
    OpAnd(TokenRange),
    OpOr(TokenRange),
    OpNot(TokenRange),
    OpGt(TokenRange),
    OpLt(TokenRange),
    OpConcat(TokenRange),
    OpAdd(TokenRange),
    OpSub(TokenRange),
    OpMul(TokenRange),
    OpDiv(TokenRange),
    OpMod(TokenRange),
    OpQ(TokenRange),
    OpColon(TokenRange),
}

impl From<Token> for Operator {
    fn from(token: Token) -> Operator {
        match token {
            Token::TokenOpPipe(tr) => Operator::OpPipe(tr),
            Token::TokenOpEq(tr) => Operator::OpEq(tr),
            Token::TokenOpNe(tr) => Operator::OpNe(tr),
            Token::TokenOpLe(tr) => Operator::OpLe(tr),
            Token::TokenOpGe(tr) => Operator::OpGe(tr),
            Token::TokenOpAnd(tr) => Operator::OpAnd(tr),
            Token::TokenOpOr(tr) => Operator::OpOr(tr),
            Token::TokenOpNot(tr) => Operator::OpNot(tr),
            Token::TokenOpGt(tr) => Operator::OpGt(tr),
            Token::TokenOpLt(tr) => Operator::OpLt(tr),
            Token::TokenOpConcat(tr) => Operator::OpConcat(tr),
            Token::TokenOpAdd(tr) => Operator::OpAdd(tr),
            Token::TokenOpSub(tr) => Operator::OpSub(tr),
            Token::TokenOpMul(tr) => Operator::OpMul(tr),
            Token::TokenOpDiv(tr) => Operator::OpDiv(tr),
            Token::TokenOpMod(tr) => Operator::OpMod(tr),
            Token::TokenOpQ(tr) => Operator::OpQ(tr),
            Token::TokenOpColon(tr) => Operator::OpColon(tr),
            _ => panic!("Unexpected operator token {:?}", token),
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum Expr {
    PrimaryExpr(ExprUnit),
    PrefixOpExpr(Operator, Box<Expr>),
    InfixOpExpr(Operator, Box<Expr>, Box<Expr>),
    PostfixOpExpr(Operator, Box<Expr>),
    TernaryOpExpr(Box<Expr>, Box<Expr>, Box<Expr>),
}

#[derive(Debug, PartialEq)]
struct TokenChunk(pub Vec<Token>);

#[derive(Debug)]
pub struct PrattParser<'a>(Iter<'a, Token>);

impl PrattParser<'_> {
    pub fn parse(chunk: &mut TokenChunk) -> Expr {
        let mut parser = PrattParser(Iter::from(&mut chunk.0));
        parser.process(0)
    }

    // operator precedence table
    fn prec(op: Operator) -> u8 {
        match op {
            // "!" => 80,
            Operator::OpNot(_) => 80,
            // "*" | "/" | "%" => 70,
            Operator::OpMul(_) => 70,
            Operator::OpDiv(_) => 70,
            Operator::OpMod(_) => 70,
            // "+" | "-" => 65,
            Operator::OpAdd(_) => 65,
            Operator::OpSub(_) => 65,
            // ">" | "<" | "<=" | ">=" => 60,
            Operator::OpGt(_) => 60,
            Operator::OpLt(_) => 60,
            Operator::OpLe(_) => 60,
            Operator::OpGe(_) => 60,
            // "==" | "!=" => 55,
            Operator::OpEq(_) => 55,
            Operator::OpNe(_) => 55,
            // "&&" => 50,
            Operator::OpAnd(_) => 50,
            // "||" => 45,
            Operator::OpOr(_) => 45,
            // "?" => 20,
            Operator::OpQ(_) => 20,
            // ":" => 15,
            Operator::OpColon(_) => 15,
            // "|" => 10,
            Operator::OpPipe(_) => 10,
            // else
            _ => 0,
        }
    }

    // (..) group sub expression
    fn group(chunk: &mut TokenChunk) -> Expr {
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
            Token::TokenPrimNumberInt(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenPrimNumberFloat(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenPrimString(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenPrimBoolean(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenPrimNull(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenFnCall(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenVar(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenRefVarSibling(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenRefVarUncle(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenRefVarRoot(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenFrameRoot(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenFrameDict(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenFrameList(..) => Expr::PrimaryExpr(token.into()),
            Token::TokenExprSequence(tokens, ..) => Self::group(&mut TokenChunk(tokens)),
            Token::TokenOpNot(..) => Expr::PrefixOpExpr(
                Operator::OpNot(token.range()),
                Box::new(self.process(Self::prec(Operator::OpNot(token.range())))),
            ),
            Token::TokenOpAdd(..) => Expr::PrefixOpExpr(
                Operator::OpAdd(token.range()),
                Box::new(self.process(Self::prec(Operator::OpAdd(token.range())))),
            ),
            Token::TokenOpSub(..) => Expr::PrefixOpExpr(
                Operator::OpSub(token.range()),
                Box::new(self.process(Self::prec(Operator::OpSub(token.range())))),
            ),
            _ => panic!("Unexpected prefix token {:?}", &token),
        };

        let mut precedence_r = self
            .peek()
            .map_or(0, |token| Self::prec(token.clone().into()));

        while prec < precedence_r {
            let token = self.take_next().unwrap();
            lhs = match token {
                Token::TokenExprSequence(mut tokens, ..) => Self::group(&mut TokenChunk(tokens)),
                Token::TokenOpOr(..) => Expr::InfixOpExpr(
                    Operator::OpOr(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpOr(token.range())))),
                ),
                Token::TokenOpAnd(..) => Expr::InfixOpExpr(
                    Operator::OpAnd(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpAnd(token.range())))),
                ),
                Token::TokenOpEq(..) => Expr::InfixOpExpr(
                    Operator::OpEq(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpEq(token.range())))),
                ),
                Token::TokenOpNe(..) => Expr::InfixOpExpr(
                    Operator::OpNe(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpNe(token.range())))),
                ),
                Token::TokenOpLt(..) => Expr::InfixOpExpr(
                    Operator::OpLt(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpLt(token.range())))),
                ),
                Token::TokenOpGt(..) => Expr::InfixOpExpr(
                    Operator::OpGt(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpGt(token.range())))),
                ),
                Token::TokenOpLe(..) => Expr::InfixOpExpr(
                    Operator::OpLe(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpLe(token.range())))),
                ),
                Token::TokenOpGe(..) => Expr::InfixOpExpr(
                    Operator::OpGe(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpGe(token.range())))),
                ),
                Token::TokenOpAdd(..) => Expr::InfixOpExpr(
                    Operator::OpAdd(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpAdd(token.range())))),
                ),
                Token::TokenOpSub(..) => Expr::InfixOpExpr(
                    Operator::OpSub(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpSub(token.range())))),
                ),
                Token::TokenOpMul(..) => Expr::InfixOpExpr(
                    Operator::OpMul(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpMul(token.range())))),
                ),
                Token::TokenOpDiv(..) => Expr::InfixOpExpr(
                    Operator::OpDiv(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpDiv(token.range())))),
                ),
                Token::TokenOpMod(..) => Expr::InfixOpExpr(
                    Operator::OpMod(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpMod(token.range())))),
                ),
                Token::TokenOpNot(..) => Expr::PrefixOpExpr(
                    Operator::OpNot(token.range()),
                    Box::new(self.process(Self::prec(Operator::OpNot(token.range())))),
                ),
                // expr ? expr : expr
                Token::TokenOpQ(..) => {
                    // -1 here makes sure that the ternary operator is right associative
                    let expr_t = self.process(Self::prec(Operator::OpQ(token.range())) - 1);
                    match self.take_next() {
                        Some(Token::TokenOpColon(..)) => Expr::TernaryOpExpr(
                            Box::new(lhs),
                            Box::new(expr_t),
                            Box::new(self.process(prec)),
                        ),
                        Some(token) => panic!("Expected ':' in ternary expr, found {:?}", token),
                        None => panic!("Unexpected EOF in ternary expr"),
                    }
                }
                Token::TokenOpPipe(..) => Expr::InfixOpExpr(
                    Operator::OpPipe(token.range()),
                    Box::new(lhs),
                    Box::new(self.process(Self::prec(Operator::OpPipe(token.range())))),
                ),
                _ => panic!("Unexpected expr-chunk token {:?}", token),
            };
            precedence_r = self
                .peek()
                .map_or(0, |token| Self::prec(token.clone().into()));
        }
        lhs
    }
}

/*
#[cfg(test)]
mod tests {
    use tokenizer::Token;

    use super::*;

    #[test]
    fn test_simple() {
        let expr_seq = vec![
            Token::TokenPrimNumberInt(1),
            Token::TokenOpAdd,
            Token::TokenRefSibling(vec![TokenKey::DummySn(0)]),
        ];
        let mut chunk = TokenChunk(expr_seq);
        let expr = PrattParser::parse(&mut chunk);
        assert_eq!(
            expr,
            Expr::InfixOpExpr(
                Operator::OpAdd,
                Box::new(Expr::PrimaryExpr(ExprUnit::UnitPrimNumberInt(1))),
                Box::new(Expr::PrimaryExpr(ExprUnit::UnitPendedRef(
                    RefType::RefSibling,
                    vec![TokenKey::DummySn(0)]
                ))),
            )
        );

        let expr_seq = vec![
            Token::TokenPrimNumberInt(1),
            Token::TokenOpSub,
            Token::TokenRefSibling(vec![TokenKey::DummySn(0)]),
        ];
        let mut chunk = TokenChunk(expr_seq);
        let expr = PrattParser::parse(&mut chunk);
        assert_eq!(
            expr,
            Expr::InfixOpExpr(
                Operator::OpSub,
                Box::new(Expr::PrimaryExpr(ExprUnit::UnitPrimNumberInt(1))),
                Box::new(Expr::PrimaryExpr(ExprUnit::UnitPendedRef(
                    RefType::RefSibling,
                    vec![TokenKey::DummySn(0)]
                ))),
            )
        );

        let expr_seq = vec![
            Token::TokenPrimNumberInt(1),
            Token::TokenOpAdd,
            Token::TokenPrimNumberInt(2),
            Token::TokenOpMul,
            Token::TokenPrimNumberInt(3),
        ];
        let mut chunk = TokenChunk(expr_seq);
        let expr = PrattParser::parse(&mut chunk);
        assert_eq!(
            expr,
            Expr::InfixOpExpr(
                Operator::OpAdd,
                Box::new(Expr::PrimaryExpr(ExprUnit::UnitPrimNumberInt(1))),
                Box::new(Expr::InfixOpExpr(
                    Operator::OpMul,
                    Box::new(Expr::PrimaryExpr(ExprUnit::UnitPrimNumberInt(2))),
                    Box::new(Expr::PrimaryExpr(ExprUnit::UnitPrimNumberInt(3))),
                )),
            )
        );
    }

    #[test]
    fn test_from_token_to_unit() {
        let token = Token::TokenPrimNumberInt(1);
        let unit = ExprUnit::UnitPrimNumberInt(1);
        assert_eq!(unit, token.into());

        let token = Token::TokenPrimNumberFloat(1.0);
        let unit = ExprUnit::UnitPrimNumberFloat(1.0);
        assert_eq!(unit, token.into());

        let token = Token::TokenPrimString("hello".to_string());
        let unit = ExprUnit::UnitPrimString("hello".to_string());
        assert_eq!(unit, token.into());

        let token = Token::TokenPrimBoolean(true);
        let unit = ExprUnit::UnitPrimBoolean(true);
        assert_eq!(unit, token.into());

        let token = Token::TokenPrimNull;
        let unit = ExprUnit::UnitPrimNull;
        assert_eq!(unit, token.into());

        let token = Token::TokenFnCall("foo".to_string(), vec![]);
        let unit = ExprUnit::UnitPendedFnCall("foo".to_string(), vec![]);
        assert_eq!(unit, token.into());

        let token = Token::TokenVar("foo".to_string());
        let unit = ExprUnit::UnitPendedVar("foo".to_string());
        assert_eq!(unit, token.into());

        let token = Token::TokenRefSibling(vec![TokenKey::DummySn(0)]);
        let unit = ExprUnit::UnitPendedRef(RefType::RefSibling, vec![TokenKey::DummySn(0)]);
        assert_eq!(unit, token.into());

        let token = Token::TokenRefUncle(vec![TokenKey::DummySn(0)]);
        let unit = ExprUnit::UnitPendedRef(RefType::RefUncle, vec![TokenKey::DummySn(0)]);
        assert_eq!(unit, token.into());

        let token = Token::TokenRefRoot(vec![TokenKey::DummySn(0)]);
        let unit = ExprUnit::UnitPendedRef(RefType::RefRoot, vec![TokenKey::DummySn(0)]);
        assert_eq!(unit, token.into());

        let token = Token::TokenFrameDict(vec![]);
        let unit = ExprUnit::UnitFrameDict(vec![]);
        assert_eq!(unit, token.into());

        let token = Token::TokenFrameList(vec![]);
        let unit = ExprUnit::UnitFrameList(vec![]);
        assert_eq!(unit, token.into());
    }
}
*/
