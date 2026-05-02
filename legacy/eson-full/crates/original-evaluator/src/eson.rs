use std::collections::HashMap;
use std::rc::Rc;

use tokenizer::{Token, TokenKey, TokenRange};

use crate::ast::{Entity, EsonEntity};
use crate::expr::ExprUnit;
use crate::util_tree::{Maintain, Node};

#[derive(Debug, Clone, PartialEq)]
pub enum EsonValue {
    EsonNumberInt(i64),
    EsonNumberFloat(f64),
    EsonString(String),
    EsonBoolean(bool),
    EsonDict(HashMap<String, EsonValue>),
    EsonList(Vec<EsonValue>),
    EsonNull,
}

/*
impl From<String> for EsonValue {
    fn from(s: String) -> EsonValue {
        let x = tokenizer::parse_prim(s.as_str()).unwrap().1;
        // let x = tokenizer::parse_prim(s.as_str()).unwrap().1;
        match x {
            Token::TokenPrimNumberInt(n) => EsonValue::EsonNumberInt(n),
            Token::TokenPrimNumberFloat(n) => EsonValue::EsonNumberFloat(n),
            Token::TokenPrimString(s) => EsonValue::EsonString(s),
            Token::TokenPrimBoolean(b) => EsonValue::EsonBoolean(b),
            Token::TokenPrimNull => EsonValue::EsonNull,
            _ => unreachable!("unexpected case {:?}", x),
        }
    }
}
*/

impl From<EsonValue> for ExprUnit {
    fn from(e: EsonValue) -> ExprUnit {
        match e {
            EsonValue::EsonNumberInt(n) => ExprUnit::UnitPrimNumberInt(n, TokenRange::default()),
            EsonValue::EsonNumberFloat(n) => ExprUnit::UnitPrimNumberFloat(n, TokenRange::default()),
            EsonValue::EsonString(s) => ExprUnit::UnitPrimString(s, TokenRange::default()),
            EsonValue::EsonBoolean(b) => ExprUnit::UnitPrimBoolean(b, TokenRange::default()),
            EsonValue::EsonDict(d) => ExprUnit::UnitFrameDict(
                d.into_iter()
                    .map(|(k, v)| (TokenKey::String(k, TokenRange::default()), v.into())) // token range is dummy
                    .collect()
                , TokenRange::default()
            ),
            EsonValue::EsonList(l) => ExprUnit::UnitFrameList(
                l.into_iter()
                    .enumerate()
                    .map(|(i, v)| (TokenKey::DummySn(i), v.into()))
                    .collect(), TokenRange::default()
            ),
            EsonValue::EsonNull => ExprUnit::UnitPrimNull(TokenRange::default()),
        }
    }
}

fn _dump_to_val(node: Rc<Node<EsonEntity>>) -> (TokenKey, EsonValue) {
    let EsonEntity(idx, entity) = node.payload().as_ref().clone();
    let children = node.children.borrow();
    match entity {
        Entity::Attachable => {
            let mut _bool_list = true;
            let mut _children = vec![];
            for child in children.iter() {
                let child_i_v = _dump_to_val(child.clone());
                // if any child_i is a string, then it is a dict
                if let TokenKey::String(..) = child_i_v.0 {
                    _bool_list = false;
                }
                _children.push(child_i_v);
            }

            return if _bool_list {
                let _c = _children.into_iter().map(|(_, v)| v).collect();
                (idx, EsonValue::EsonList(_c))
            } else {
                let _c = _children
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect();
                (idx, EsonValue::EsonDict(_c))
            };
        }
        Entity::Lone(u) => match u {
            ExprUnit::UnitPrimNumberInt(n, _) => (idx, EsonValue::EsonNumberInt(n)),
            ExprUnit::UnitPrimNumberFloat(n, _) => (idx, EsonValue::EsonNumberFloat(n)),
            ExprUnit::UnitPrimString(s,_) => (idx, EsonValue::EsonString(s.clone())),
            ExprUnit::UnitPrimBoolean(b, _) => (idx, EsonValue::EsonBoolean(b)),
            ExprUnit::UnitPrimNull(_) => (idx, EsonValue::EsonNull),
            _ => unreachable!("unexpected case {:?}", u),
        },
    }
}

fn _render_to_json(v: EsonValue) -> String {
    match v {
        EsonValue::EsonNumberInt(n) => n.to_string(),
        EsonValue::EsonNumberFloat(n) => n.to_string(),
        EsonValue::EsonString(s) => format!("\"{}\"", s),
        EsonValue::EsonBoolean(b) => b.to_string(),
        EsonValue::EsonDict(d) => {
            let mut s = "{".to_string();
            for (k, v) in d {
                s.push_str(&format!("\"{}\":{},", k, _render_to_json(v)));
            }
            s.push_str("}");
            s
        }
        EsonValue::EsonList(l) => {
            let mut s = "[".to_string();
            for v in l {
                s.push_str(&format!("{},", _render_to_json(v)));
            }
            s.push_str("]");
            s
        }
        EsonValue::EsonNull => "null".to_string(),
    }
}

fn _pretty(json_string: String) -> String {
    let mut s = json_string.chars().peekable();
    let mut indent = 0;
    let mut out = String::new();
    while let Some(c) = s.next() {
        match c {
            '{' | '[' => {
                out.push(c);
                out.push('\n');
                indent += 1;
                for _ in 0..indent {
                    out.push_str("  ");
                }
            }
            '}' | ']' => {
                // remove last comma, space or newline before closing bracket
                while let Some(s) = out.pop() {
                    if s != ' ' && s != '\n' && s != ',' {
                        out.push(s);
                        break;
                    }
                }
                out.push('\n');
                indent -= 1;
                for _ in 0..indent {
                    out.push_str("  ");
                }
                out.push(c);
            }
            ',' => {
                out.push(c);
                out.push('\n');
                for _ in 0..indent {
                    out.push_str("  ");
                }
            }
            _ => out.push(c),
        }
    }
    out
}

pub trait Render {
    fn dump(&self) -> EsonValue;
    fn render_to_json(&self) -> String;
    fn render_to_pretty_json(&self) -> String;
}

impl Render for Rc<Node<EsonEntity>> {
    fn dump(&self) -> EsonValue {
        let (_, val) = _dump_to_val(self.clone());
        val
    }

    fn render_to_json(&self) -> String {
        let val = self.dump();
        _render_to_json(val)
    }

    fn render_to_pretty_json(&self) -> String {
        let val = self.dump();
        _pretty(_render_to_json(val))
    }
}

/*
#[cfg(test)]
mod tests {
    use crate::ast::ast;
    use crate::context::Context;
    use crate::eval::Eval;

    use super::*;

    #[test]
    fn test_simple3() {
        let str = r##"
        {
            0: 1,
            b1: 2,
            "c": 3
        }"##;
        let mut ctx = Context::default();
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        ast.eval(&mut ctx);
        // dbg!(&ast);
        // dbg!(ast.dump());
        // dbg!(ast.render_to_json());
        println!("{}", ast.render_to_pretty_json());
    }

    #[test]
    fn test_simple2() {
        let str = r##"
        [1, 2, 3]"##;
        let mut ctx = Context::default();
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        ast.eval(&mut ctx);
        dbg!(&ast);
        dbg!(ast.dump());
    }

    #[test]
    fn test_simple() {
        let str = r##"
        {
            "a": 1,
            "b": 2,
            "c": 3
        }"##;
        let mut ctx = Context::default();
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        ast.eval(&mut ctx);
        dbg!(&ast);
        dbg!(ast.dump());
    }
}
*/