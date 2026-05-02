use std::collections::HashMap;

use example_evaluator::context::Context;
use example_evaluator::ops::Add;
use parser::{Eson, EsonNumeric, EsonRef, EsonVal, FmtString, PrattParser, RefIndex, TokenChunk};
use serde::Serialize;
use serde_json::Value;
use tests::traveler::{EsonValTraveler, Travel};

//
#[derive(Debug, Serialize, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Boolean(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

#[derive(Debug)]
struct Evaluator {
    ctx: Context,
}

//
impl Evaluator {
    pub fn new(ctx: Context) -> Self {
        Self { ctx }
    }

    fn compute(&self, eson: &mut EsonVal, t: &EsonValTraveler) -> JsonValue {
        match eson {
            EsonVal::Null => JsonValue::Null,
            EsonVal::Boolean(b) => JsonValue::Boolean(*b),
            EsonVal::Int(i) => JsonValue::Number(*i as f64),
            EsonVal::Float(f) => JsonValue::Number(*f),
            EsonVal::String(s) => JsonValue::String(s.clone()),
            EsonVal::List(list) => {
                JsonValue::Array(list.into_iter().map(|v| self.compute(v, t)).collect())
            }
            EsonVal::Dict(dict) => JsonValue::Object(
                dict.into_iter()
                    .map(|(k, v)| (k.to_string(), self.compute(v, t)))
                    .collect(),
            ),
            EsonVal::FnCall(name, args) => todo!(),
            EsonVal::FmtString(s) => {
                // iter s, map, collect, join as string
                JsonValue::String(
                    s.into_iter()
                        .map(|v| match v {
                            FmtString::Lit(s) => s.clone(),
                            FmtString::Expr(tc) => {
                                // how to covert &TokenChunk to &mut TokenChunk?
                                // expr(input: &mut TokenChunk)
                                String::from(self.expr(tc))
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("")
                        .into(),
                )
            }
            EsonVal::Var(v) => {
                if let Some(v) = self.ctx.var(v) {
                    return self.compute(v, t);
                }
                JsonValue::Null
            }
            EsonVal::Ref(r) => {
                t.move_to(&t.abs(&mut r.clone()));
                if let Ok(ev) = t.value() {
                    return self.compute(ev, t);
                }
                JsonValue::Null
            }
        }
    }

    pub fn eval(&self, eson: &mut EsonVal) -> JsonValue {
        let t = EsonValTraveler::from(eson);
        self.compute(eson, &t)
    }

    fn expr(&self, tc: &mut TokenChunk) -> EsonVal {
        // use std::mem::replace translate &mut TokenChunk to TokenChunk
        // let tc = std::mem::replace(tc, TokenChunk::new());
        let expr = PrattParser::parse(tc);
        dbg!(expr);

        EsonVal::Null
    }
}

//
// impl Add for EsonNumeric {
//     type Output = Self;
//
//     fn add(self, rhs: Self) -> Self::Output {
//         match self {
//             EsonNumeric::Int(i) => match rhs {
//                 EsonNumeric::Int(j) => {
//                     return EsonNumeric::Int(i + j);
//                 }
//                 EsonNumeric::Float(f) => {
//                     return EsonNumeric::Float(i as f64 + f);
//                 }
//             },
//             EsonNumeric::Float(f) => match rhs {
//                 EsonNumeric::Int(i) => {
//                     return EsonNumeric::Float(f + i as f64);
//                 }
//                 EsonNumeric::Float(j) => {
//                     return EsonNumeric::Float(f + j);
//                 }
//             },
//         }
//     }
// }
//
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_string() {
        let test_dat = r#"{
            "fmt": "My name is ${ name }, I'm ${ age } years old"
        }"#;

        let (_, mut r) = parser::eson_val(test_dat).unwrap();
        let e = Evaluator::new(Context::new());
        let v = e.eval(&mut r);
        assert_eq!(
            v,
            JsonValue::Object(
                vec![(
                         "fmt".to_string(),
                         JsonValue::String("My name is John, I'm 17 years old".to_string())
                     ), ]
                    .into_iter()
                    .collect(),
            )
        )
    }

    #[test]
    fn test_var() {
        let test_dat = r#"{
            "name": "John",
            "age": age
        }"#;

        let (_, r) = parser::eson_val(test_dat).unwrap();
        let e = Evaluator::new(Context::new());
        let v = e.eval(&r);
        dbg!(v);
    }

    #[test]
    fn test_eval_ref() {
        let test_dat = r#"{
            "name": "John",
            "get_neighbor_name": $.name
        }"#;

        let (_, r) = parser::eson_val(test_dat).unwrap();
        let e = Evaluator::new(Context::new());
        let v = e.eval(&r);
        assert_eq!(
            v,
            JsonValue::Object(
                vec![
                    ("name".to_string(), JsonValue::String("John".to_string())),
                    (
                        "get_neighbor_name".to_string(),
                        JsonValue::String("John".to_string())
                    ),
                ]
                    .into_iter()
                    .collect(),
            )
        )
    }

    #[test]
    fn test_eval_smoke() {
        let test_dat = r#"{
            "name": "John",
        }"#;

        let (_, r) = parser::eson_val(test_dat).unwrap();
        let e = Evaluator::new(Context::new());
        let v = e.eval(&r);
        assert_eq!(
            v,
            JsonValue::Object(
                vec![("name".to_string(), JsonValue::String("John".to_string()))]
                    .into_iter()
                    .collect(),
            )
        )
    }
    //
    //     #[test]
    //     fn test_query() {
    //         // dbg!(r);
    //         let e = Evaluator {
    //             ctx: Context::new(),
    //             val: Rc::new(RefCell::new(JsonValue::Object(
    //                 vec![
    //                     ("name".to_string(), JsonValue::String("John".to_string())),
    //                     (
    //                         "age".to_string(),
    //                         JsonValue::Object(
    //                             vec![
    //                                 ("value".to_string(), JsonValue::Number(42.0)),
    //                                 ("unit".to_string(), JsonValue::String("year".to_string())),
    //                             ]
    //                             .into_iter()
    //                             .collect(),
    //                         ),
    //                     ),
    //                     (
    //                         "city".to_string(),
    //                         JsonValue::Array(
    //                             vec![
    //                                 JsonValue::String("London".to_string()),
    //                                 JsonValue::String("New York".to_string()),
    //                                 JsonValue::String("Paris".to_string()),
    //                             ]
    //                             .into_iter()
    //                             .collect(),
    //                         ),
    //                     ),
    //                 ]
    //                 .into_iter()
    //                 .collect(),
    //             ))),
    //             pos: Rc::new(RefCell::new(vec![])),
    //         };
    //
    //         // dbg!(&e.val);
    //
    //         e.move_to(&[RefIndex::Str(String::from("name"))]);
    //         assert_eq!(e.value().unwrap(), JsonValue::String("John".to_string()));
    //
    //         e.move_to(&[
    //             RefIndex::Str(String::from("age")),
    //             RefIndex::Str(String::from("value")),
    //         ]);
    //         assert_eq!(e.value().unwrap(), JsonValue::Number(42.0));
    //
    //         e.move_to(&[RefIndex::Str(String::from("city")), RefIndex::Int(1)]);
    //         assert_eq!(
    //             e.value().unwrap(),
    //             JsonValue::String("New York".to_string())
    //         );
    //
    //         e.move_to(&[RefIndex::Str(String::from("not_exist"))]);
    //         assert!(e.value().is_err());
    //     }
    //
    //     #[test]
    //     fn test_eval() {
    //         let test_dat = r#"{
    //             "name": "John",
    //             "get_neighbor_name": $.name
    //         }"#;
    //
    //         let (_, r) = root(test_dat).unwrap();
    //         // dbg!(&r);
    //
    //         let mut e = Evaluator::new();
    //         e.eval(r);
    //
    //         e.move_to(&[RefIndex::Str(String::from("get_neighbor_name"))]);
    //         let v = e.value().unwrap();
    //         assert_eq!(v, JsonValue::String("John".to_string()));
    //     }
    //
    //     #[test]
    //     fn test_add() {
    //         let input = r##"{ a: f(1 + 2) }"##;
    //         let (remaining, expr) = eson_val(input).unwrap();
    //         dbg!(expr);
    //     }
    //
    //     /*
    //
    //     #[test]
    //     fn test_ref1() {
    //         let test_dat = r#"{
    //             "name": "John",
    //             "get_neighbor_name": ${ super.name }
    //         }"#;
    //
    //         let (remaining, expr) = eson(test_dat).unwrap();
    //         match expr {
    //             EsonValue::Dict(obj) => {
    //                 // 遍历 obj, 如果遇到 ExprValue, 则计算
    //                 for (k, v) in obj {
    //                     match v {
    //                         EsonValue::Expr(chunk) => {
    //                             let mut parser = Parser::new(chunk.into());
    //                             let chunk = parser.parse(0);
    //                             let v = chunk.compute();
    //                             dbg!(v);
    //                             // chunk = Primary(
    //                             //     Ref(
    //                             //         Super(
    //                             //             [
    //                             //                 Str(
    //                             //                     "name",
    //                             //                 ),
    //                             //             ],
    //                             //         ),
    //                             //     ),
    //                             // )
    //
    //
    //                             // match chunk {
    //                             //     ExprChunk::Primary(ExprToken::Ref(r)) => {
    //                             //         match r {
    //                             //             RefPronoun::Super(r) => {
    //                             //                 assert_eq!(r, "name");
    //                             //                 assert_eq!(query(&RefPronoun::Super(r.to_string())), EsonValue::Str(String::from("John")));
    //                             //             }
    //                             //             _ => todo!(),
    //                             //         }
    //                             //     }
    //                             //     _ => todo!(),
    //                             // }
    //                         }
    //                         _ => {
    //                             dbg!(v);
    //                         }
    //                     }
    //                 }
    //             }
    //             _ => todo!(),
    //         }
    //
    //     }
    //
    //     #[test]
    //     fn test_var() {
    //         let input = "${ name }";
    //         let (remaining, expr) = eson(input).unwrap();
    //         match expr {
    //             EsonValue::Expr(chunk) => {
    //                 assert_eq!(
    //                     chunk,
    //                     ExprTokenChunk::from(vec![ExprToken::Var("name".to_string()),])
    //                 );
    //
    //                 let mut parser = Parser::new(chunk.into());
    //                 // dbg!(parser);
    //                 // => parser = Parser(
    //                 //     Iter {
    //                 //         inner: [
    //                 //             Var(
    //                 //                 "name",
    //                 //             ),
    //                 //         ],
    //                 //         cursor: 0,
    //                 //     },
    //                 // )
    //                 let chunk = parser.parse(0);
    //                 // dbg!(chunk);
    //                 assert_eq!(
    //                     chunk,
    //                     ExprChunk::Primary(ExprToken::Var("name".to_string()))
    //                 );
    //
    //                 match chunk {
    //                     ExprChunk::Primary(ExprToken::Var(r)) => {
    //                         // dbg!(r); // => r = "name"
    //                         // assert_eq!(r, "name");
    //                         // dbg!(var(r.as_str()));
    //                         assert_eq!(var(r.as_str()), EsonValue::Str(String::from("John")));
    //                     }
    //                     _ => todo!(),
    //                 }
    //             }
    //             _ => todo!(),
    //         }
    //     }
    //
    //     #[test]
    //     fn test_add() {
    //         let input = "${ 1 + 2 }";
    //         let (remaining, expr) = eson(input).unwrap();
    //         // expr = Expr(
    //         //     ExprTokenChunk(
    //         //         [
    //         //             Val(
    //         //                 Int(
    //         //                     1,
    //         //                 ),
    //         //             ),
    //         //             Plus,
    //         //             Val(
    //         //                 Int(
    //         //                     2,
    //         //                 ),
    //         //             ),
    //         //         ],
    //         //     ),
    //         match expr {
    //             EsonValue::Expr(chunk) => {
    //                 assert_eq!(
    //                     chunk,
    //                     ExprTokenChunk::from(vec![
    //                         ExprToken::Val(EsonValue::Int(1)),
    //                         ExprToken::Plus,
    //                         ExprToken::Val(EsonValue::Int(2)),
    //                     ])
    //                 );
    //
    //                 let mut parser = Parser::new(chunk.into());
    //                 let chunk = parser.parse(0);
    //                 assert_eq!(
    //                     chunk,
    //                     ExprChunk::InfixOp(
    //                         ExprToken::Plus,
    //                         Box::new(ExprChunk::Primary(ExprToken::Val(EsonValue::Int(1)))),
    //                         Box::new(ExprChunk::Primary(ExprToken::Val(EsonValue::Int(2)))),
    //                     )
    //                 );
    //
    //                 match chunk {
    //                     ExprChunk::InfixOp(op, lhs, rhs) => {
    //                         match op {
    //                             ExprToken::Plus => {
    //                                 dbg!("look here");
    //
    //                                 // if let (
    //                                 //     ExprChunk::Primary(ExprToken::Val(r))
    //                                 //     // ExprChunk::Primary(ExprToken::Val(r)),
    //                                 // ) = *rhs {
    //                                 //     dbg!(r);
    //                                 //     // dbg!(r);
    //                                 // }
    //                             }
    //                             _ => todo!(),
    //                         }
    //                     }
    //                     _ => todo!(),
    //                 }
    //             }
    //             _ => todo!(),
    //         }
    //     }
    //
    //
    //     #[test]
    //     fn test() {
    //         let dat = "${ 1 + 2 * 3 }";
    //         if let Ok((remaining, expr)) = eson(dat) {
    //             match expr {
    //                 EsonValue::Expr(chunk) => {
    //                     assert_eq!(
    //                         chunk,
    //                         ExprTokenChunk::from(vec![
    //                             ExprToken::Val(EsonValue::Int(1)),
    //                             ExprToken::Plus,
    //                             ExprToken::Val(EsonValue::Int(2)),
    //                             ExprToken::Mul,
    //                             ExprToken::Val(EsonValue::Int(3)),
    //                         ])
    //                     );
    //
    //                     let mut parser = Parser::new(chunk.into());
    //                     let chunk = parser.parse(0);
    //                     assert_eq!(
    //                         chunk,
    //                         ExprChunk::InfixOp(
    //                             ExprToken::Plus,
    //                             Box::new(ExprChunk::Primary(ExprToken::Val(EsonValue::Int(1)))),
    //                             Box::new(ExprChunk::InfixOp(
    //                                 ExprToken::Mul,
    //                                 Box::new(ExprChunk::Primary(ExprToken::Val(EsonValue::Int(2)))),
    //                                 Box::new(ExprChunk::Primary(ExprToken::Val(EsonValue::Int(3)))),
    //                             )),
    //                         )
    //                     );
    //
    //                     match chunk {
    //                         ExprChunk::InfixOp(op, lhs, rhs) => {
    //                             match op {
    //                                 ExprToken::Plus => {
    //                                     // if let (
    //                                     //     ExprChunk::Primary(ExprToken::Val(r))
    //                                     //     // ExprChunk::Primary(ExprToken::Val(r)),
    //                                     // ) = *rhs {
    //                                     //     dbg!(r);
    //                                     //     // dbg!(r);
    //                                     // }
    //                                 }
    //                                 _ => todo!(),
    //                             }
    //                         }
    //                         _ => todo!(),
    //                     }
    //                 }
    //                 _ => todo!(),
    //             }
    //         }
    //     }
    //
    //     */
}
