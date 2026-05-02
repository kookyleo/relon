use std::ops::Deref;
use std::rc::Rc;

use crate::ast::{Entity, EsonEntity, AST};
use crate::context::{CircularRefDetector, Context, Var};
use crate::error::{
    new_circular_ref_error, new_invalid_ref_error, new_undefined_variable_error, EvaluatorError,
};
use crate::expr::{Expr, ExprUnit, Operator, RefType};
use crate::ops::*;
use crate::udf::udf_call;
use crate::util_tree::{Maintain, Navigate, Node};

pub(crate) fn compute(expr: Expr, ctx: &mut Context) -> ExprUnit {
    match expr {
        Expr::PrimaryExpr(u) => u,
        Expr::PrefixOpExpr(op, expr) => {
            // ops: + - !
            match op {
                Operator::OpAdd(_) => compute(*expr, ctx).pos(), // positive +
                Operator::OpSub(_) => compute(*expr, ctx).neg(), // negative -
                Operator::OpNot(_) => compute(*expr, ctx).not(), // not !
                _ => unreachable!("unexpected case"),
            }
        }
        Expr::InfixOpExpr(op, expr1, expr2) => {
            // ops: + - * / % == != < <= > >= && || |
            match op {
                Operator::OpAdd(_) => compute(*expr1, ctx).add(compute(*expr2, ctx)), // add +
                Operator::OpSub(_) => compute(*expr1, ctx).sub(compute(*expr2, ctx)), // sub -
                Operator::OpMul(_) => compute(*expr1, ctx).mul(compute(*expr2, ctx)), // multiply *
                Operator::OpDiv(_) => compute(*expr1, ctx).div(compute(*expr2, ctx)), // divide /
                Operator::OpMod(_) => compute(*expr1, ctx).mo_(compute(*expr2, ctx)), // mod %
                Operator::OpEq(_) => compute(*expr1, ctx).eq_(compute(*expr2, ctx)),  // equal ==
                Operator::OpNe(_) => compute(*expr1, ctx).ne_(compute(*expr2, ctx)),  // not equal !=
                Operator::OpLt(_) => compute(*expr1, ctx).lt_(compute(*expr2, ctx)),  // less than <
                Operator::OpLe(_) => compute(*expr1, ctx).le_(compute(*expr2, ctx)),  // less equal <=
                Operator::OpGt(_) => compute(*expr1, ctx).gt_(compute(*expr2, ctx)),  // greater than >
                Operator::OpGe(_) => compute(*expr1, ctx).ge_(compute(*expr2, ctx)), // greater equal >=
                Operator::OpAnd(_) => compute(*expr1, ctx).and(compute(*expr2, ctx)), // and &&
                Operator::OpOr(_) => compute(*expr1, ctx).or(compute(*expr2, ctx)),  // or ||
                Operator::OpPipe(_) => compute(*expr1, ctx).pipe(compute(*expr2, ctx)), // pipe |
                _ => unreachable!("unexpected case"),
            }
        }
        Expr::PostfixOpExpr(op, expr) => {
            // ops:
            match op {
                _ => unreachable!("unexpected case"),
            }
        }
        Expr::TernaryOpExpr(expr, expr1, expr2) => {
            compute(*expr, ctx).ternary(compute(*expr1, ctx), compute(*expr2, ctx))
        }
    }
}

pub trait Eval {
    fn eval(&self, ctx: &mut Context) -> Result<(), EvaluatorError>;
}

impl Eval for Rc<Node<EsonEntity>> {
    fn eval(&self, ctx: &mut Context) -> Result<(), EvaluatorError> {
        // abort if circular ref detected
        if ctx.is_circular_ref_detected(self) {
            return Err(new_circular_ref_error(ctx.get_circle_ref_nodes()));
        }
        let binding = self.payload();
        let EsonEntity(idx, entity) = binding.as_ref();
        match entity {
            Entity::Attachable => {
                for child in self.children.borrow().iter() {
                    if let Err(e) = child.eval(ctx) {
                        return Err(e);
                    }
                }
                Ok(())
            }
            Entity::Lone(u) => match u {
                ExprUnit::UnitPrimNumberInt(n, tr) => {
                    Ok(self.update(idx.clone(), ExprUnit::UnitPrimNumberInt(*n, tr.clone())))
                }
                ExprUnit::UnitPrimNumberFloat(n, tr) => {
                    Ok(self.update(idx.clone(), ExprUnit::UnitPrimNumberFloat(*n, tr.clone())))
                }
                ExprUnit::UnitPrimString(s, tr) => {
                    Ok(self.update(idx.clone(), ExprUnit::UnitPrimString(s.clone(), tr.clone())))
                }
                ExprUnit::UnitPrimBoolean(b, tr) => {
                    Ok(self.update(idx.clone(), ExprUnit::UnitPrimBoolean(*b, tr.clone())))
                }
                ExprUnit::UnitPrimNull(tr) => Ok(self.update(idx.clone(), ExprUnit::UnitPrimNull(tr.clone()))),
                ExprUnit::UnitPendedExpr(e, _) => {
                    Ok(self.update(idx.clone(), compute(*e.clone(), ctx)))
                }
                ExprUnit::UnitPendedFnCall(name, args, _) => match udf_call(name, args.clone()) {
                    Ok(r) => Ok(self.update(idx.clone(), r)),
                    Err(e) => Err(e),
                },
                ExprUnit::UnitPendedVar(v, _) => match ctx.get_var(v.clone()) {
                    Some(x) => Ok(self.update(idx.clone(), x.into())),
                    None => Err(new_undefined_variable_error(v.clone(), self.full_name())),
                },
                ExprUnit::UnitPendedRef(rt, ii, _) => {
                    let ref_node = match rt {
                        RefType::RefSibling => self.nav_from_sibling(ii.clone()),
                        RefType::RefUncle => self.nav_from_uncle(ii.clone()),
                        RefType::RefRoot => self.nav_to_root().nav_to_descendant(ii.clone()),
                    };
                    match ref_node {
                        Some(x) => {
                            ctx.circular_ref_detection_sign(&self);
                            if let Err(e) = x.eval(ctx) {
                                return Err(e);
                            }
                            ctx.circular_ref_detection_sign_clear();

                            match x.payload().as_ref() {
                                EsonEntity(_, Entity::Lone(eu)) => {
                                    Ok(self.update(idx.clone(), eu.clone()))
                                }
                                EsonEntity(_, Entity::Attachable) => {
                                    self.update_payload(EsonEntity(
                                        idx.clone(),
                                        Entity::Attachable,
                                    ));
                                    for child in x.children.borrow().iter() {
                                        child.clone().attached_to(self);
                                    }
                                    Ok(())
                                }
                            }
                        }
                        None => Err(new_invalid_ref_error(ii, self.full_name())),
                    }
                }
                _ => unimplemented!("{:?} should not be here", u),
            },
        }
    }
}

/*
#[cfg(test)]
mod tests {
    use crate::ast::ast;
    use crate::dump_to_json;
    use crate::eson::EsonValue;

    use super::*;

    #[test]
    fn test_var() {
        let mut ctx = Context::default();
        ctx.set_var(String::from("foo"), EsonValue::EsonNumberInt(1));
        let str = r#"{
            "k": foo,
        }"#;
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        let expr = ast.eval(&mut ctx);
        dbg!(ast);
    }

    #[test]
    fn test_fn_call() {
        let mut ctx = Context::default();
        let str = r#"{
            @foo
            "k": fn_double(1),
        }"#;
        let tokens = tokenizer::parse_base(str).unwrap().1;

        let mut ast = ast(tokens);
        let expr = ast.eval(&mut ctx);
        dbg!(ast);
    }

    #[test]
    fn test_circle_ref2() {
        let str = r#"{
            "n": &sibling.m,
            "m": &sibling.n,
        }"#;
        let x = dump_to_json(str, vec![], false).unwrap();
    }

    #[test]
    // #[should_panic(expected = "loop detected")]
    fn test_circular_ref() {
        let mut ctx = Context::default();
        let str = r#"{
            "n": &sibling.m,
            "m": &sibling.n,
        }"#;
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        let r = ast.eval(&mut ctx);
        match r {
            Err(e) => {
                assert_eq!(e.to_string(), "Circular ref detected: root.n, root.m");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_ref_to_fn_call() {
        let mut ctx = Context::default();
        let str = r#"{
            "n": &sibling.m,
            "m": &sibling.y,
            "x": fn_double(1),
        }"#;
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        let expr = ast.eval(&mut ctx);
        dbg!(ast);
    }

    #[test]
    fn test_simple() {
        let mut ctx = Context::default();
        let str = r#"{
            @foo
            "k": 1 + 2,
            "n": &sibling.k,
            "m": &sibling.n,
        }"#;
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        let expr = ast.eval(&mut ctx);
        dbg!(ast);
    }

    #[test]
    fn test_simple2() {
        let mut ctx = Context::default();
        let str = r#"{
            @foo
            "k": [1, 2, 3],
            "n": &sibling.k,
        }"#;
        let tokens = tokenizer::parse_base(str).unwrap().1;
        let mut ast = ast(tokens);
        let expr = ast.eval(&mut ctx);
        dbg!(ast);
    }
}
*/
