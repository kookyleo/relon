//! Binary and unary operator evaluation.
//!
//! Covers the arithmetic, comparison, and structural-merge operators that
//! reach `eval_internal` via [`relon_parser::Expr::Binary`] / `Expr::Unary`.
//! The short-circuiting `&&` / `||` and the `|` pipe live in `eval.rs`
//! because their evaluation order interacts with the dispatcher.

use crate::error::RuntimeError;
use crate::eval::Evaluator;
use crate::scope::Scope;
use crate::value::Value;
use ordered_float::OrderedFloat;
use relon_parser::{Expr, Node, Operator, TokenRange, TypeNode};
use std::sync::Arc;

#[derive(Clone, Copy)]
enum NumericValue {
    Int(i64),
    Float(OrderedFloat<f64>),
}

impl NumericValue {
    fn as_f64(self) -> f64 {
        match self {
            Self::Int(value) => value as f64,
            Self::Float(value) => value.into_inner(),
        }
    }
}

impl Evaluator<'_> {
    pub(crate) fn eval_binary(
        &self,
        op: Operator,
        left: &Node,
        right: &Node,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        let l = self.eval(left, scope)?;

        // `Schema + Dict_AST` / `Schema + (Schema + Dict_AST)`: walk
        // the RHS as a schema definition rather than evaluating it as
        // data. Typed entries become / refine schema fields; untyped
        // literal entries become defaults.
        //
        // Pure-`Dict` RHS uses the dedicated `merge_schema_with_dict_pairs`
        // (it has hybrid "field def vs default value" dispatch). Nested
        // `Schema + ... + Dict` shapes are lowered through the analyzer's
        // `lower_schema_pure` so the same desugar logic that powers
        // `@schema X: Base + { ... }` applies here too.
        if matches!(op, Operator::Add)
            && matches!(&l, Value::Schema(_))
            && matches!(
                right.expr.as_ref(),
                Expr::Dict(_) | Expr::Binary(Operator::Add, _, _)
            )
        {
            let Value::Schema(base_fields) = l else {
                unreachable!()
            };
            let merged_fields = match right.expr.as_ref() {
                Expr::Dict(pairs) => {
                    self.merge_schema_with_dict_pairs(base_fields, pairs, scope)?
                }
                _ => {
                    // Lower the nested RHS as a schema, then build it
                    // with the live scope and merge into the base.
                    let (lowered, _diags) = relon_analyzer::lower_schema_pure(None, right);
                    let r_fields = match lowered {
                        Some(def) => self.build_schema_from_def(&def, scope)?,
                        None => {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "Schema or Dict".to_string(),
                                found: "non-schema expression".to_string(),
                                range: right.range,
                            });
                        }
                    };
                    let mut merged = base_fields;
                    crate::schema::merge_schema_fields(&mut merged, r_fields);
                    merged
                }
            };
            return Ok(Value::Schema(merged_fields));
        }

        let r = self.eval(right, scope)?;
        match (op, &l, &r) {
            (Operator::Add, Value::Dict(_), Value::Dict(_)) => {
                let mut merged = l.clone();
                merged.deep_merge(&r);
                self.check_value_size(&merged, left.range)?;

                if let Value::Dict(ref d) = merged {
                    if let Some(ref brand_name) = d.brand {
                        if let Some(Value::Schema(_)) = scope.get_local(brand_name) {
                            let mut to_check = merged.clone();
                            let type_node = TypeNode {
                                path: vec![brand_name.clone()],
                                generics: Vec::new(),
                                is_optional: false,
                                range: left.range,
                                variant_fields: None,
                                doc_comment: None,
                            };
                            self.check_type(&mut to_check, &type_node, scope, left.range)?;
                            return Ok(to_check);
                        }
                    }
                }
                Ok(merged)
            }
            (Operator::Add, Value::Schema(_), Value::Schema(_)) => {
                let mut merged = l.clone();
                merged.deep_merge(&r);
                Ok(merged)
            }
            (Operator::Add, Value::String(a), b) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add, a, Value::String(b)) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add | Operator::Sub | Operator::Mul, _, _) => {
                eval_numeric_arithmetic(op, &l, left.range, &r, right.range)
            }
            (Operator::Div | Operator::Mod, _, _) => {
                eval_numeric_division(op, &l, left.range, &r, right.range)
            }
            (Operator::Eq, a, b) => Ok(Value::Bool(a == b)),
            (Operator::Ne, a, b) => Ok(Value::Bool(a != b)),
            (Operator::Lt | Operator::Gt | Operator::Le | Operator::Ge, _, _) => {
                eval_numeric_comparison(op, &l, left.range, &r, right.range)
            }
            _ => Err(RuntimeError::UnsupportedOperator(
                format!("{:?}", op),
                left.range,
            )),
        }
    }

    pub(crate) fn eval_unary(
        &self,
        op: Operator,
        node: &Node,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        let val = self.eval(node, scope)?;
        match (op, val) {
            (Operator::Not, v) => Ok(Value::Bool(!v.is_truthy())),
            (Operator::Sub, Value::Int(i)) => Ok(Value::Int(-i)),
            (Operator::Sub, Value::Float(f)) => Ok(Value::Float(-f)),
            (Operator::Sub, v) => Err(RuntimeError::TypeMismatch {
                expected: "Number".to_string(),
                found: v.type_name().to_string(),
                range: node.range,
            }),
            _ => Err(RuntimeError::UnsupportedOperator(
                format!("{:?}", op),
                node.range,
            )),
        }
    }
}

fn eval_numeric_arithmetic(
    op: Operator,
    left: &Value,
    left_range: TokenRange,
    right: &Value,
    right_range: TokenRange,
) -> Result<Value, RuntimeError> {
    let left = expect_number(left, left_range)?;
    let right = expect_number(right, right_range)?;
    match (op, left, right) {
        (Operator::Add, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a + b)),
        (Operator::Sub, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a - b)),
        (Operator::Mul, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a * b)),
        (Operator::Add, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() + b.as_f64()))),
        (Operator::Sub, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() - b.as_f64()))),
        (Operator::Mul, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() * b.as_f64()))),
        _ => unreachable!("non-arithmetic operator passed to eval_numeric_arithmetic"),
    }
}

fn eval_numeric_division(
    op: Operator,
    left: &Value,
    left_range: TokenRange,
    right: &Value,
    right_range: TokenRange,
) -> Result<Value, RuntimeError> {
    let left = expect_number(left, left_range)?;
    let right = expect_number(right, right_range)?;
    if right.as_f64() == 0.0 {
        return Err(RuntimeError::DivisionByZero(right_range));
    }
    match (op, left, right) {
        (Operator::Div, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a / b)),
        (Operator::Mod, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a % b)),
        (Operator::Div, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() / b.as_f64()))),
        (Operator::Mod, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() % b.as_f64()))),
        _ => unreachable!("non-division operator passed to eval_numeric_division"),
    }
}

fn eval_numeric_comparison(
    op: Operator,
    left: &Value,
    left_range: TokenRange,
    right: &Value,
    right_range: TokenRange,
) -> Result<Value, RuntimeError> {
    let left = expect_number(left, left_range)?.as_f64();
    let right = expect_number(right, right_range)?.as_f64();
    let result = match op {
        Operator::Lt => left < right,
        Operator::Gt => left > right,
        Operator::Le => left <= right,
        Operator::Ge => left >= right,
        _ => unreachable!("non-comparison operator passed to eval_numeric_comparison"),
    };
    Ok(Value::Bool(result))
}

fn expect_number(value: &Value, range: TokenRange) -> Result<NumericValue, RuntimeError> {
    match value {
        Value::Int(value) => Ok(NumericValue::Int(*value)),
        Value::Float(value) => Ok(NumericValue::Float(*value)),
        _ => Err(RuntimeError::TypeMismatch {
            expected: "Number".to_string(),
            found: value.type_name().to_string(),
            range,
        }),
    }
}
