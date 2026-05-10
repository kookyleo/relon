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

impl Evaluator {
    pub(crate) fn eval_binary(
        &self,
        op: Operator,
        range: TokenRange,
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
        // `#schema X: Base + { ... }` applies here too.
        if matches!(op, Operator::Add)
            && matches!(&l, Value::Schema { .. })
            && matches!(
                right.expr.as_ref(),
                Expr::Dict(_) | Expr::Binary(Operator::Add, _, _)
            )
        {
            let Value::Schema {
                fields: base_fields,
                ..
            } = l
            else {
                unreachable!()
            };
            let merged_fields = match right.expr.as_ref() {
                Expr::Dict(pairs) => {
                    self.merge_schema_with_dict_pairs(base_fields, pairs, scope)?
                }
                _ => {
                    // Lower the nested RHS as a schema, then build it
                    // with the live scope and merge into the base.
                    let (lowered, _diags) =
                        relon_analyzer::lower_schema_pure(None, Vec::new(), right);
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
            return Ok(Value::Schema {
                generics: Vec::new(),
                fields: merged_fields,
            });
        }

        let r = self.eval(right, scope)?;
        match (op, &l, &r) {
            (Operator::Add, Value::Dict(_), Value::Dict(_)) => {
                let mut merged = l.clone();
                merged.deep_merge(&r);
                self.check_value_size(&merged, left.range)?;

                if let Value::Dict(ref d) = merged {
                    if let Some(ref brand_name) = d.brand {
                        if let Some(Value::Schema { .. }) = scope.get_local(brand_name) {
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
            (Operator::Add, Value::Schema { .. }, Value::Schema { .. }) => {
                let mut merged = l.clone();
                merged.deep_merge(&r);
                Ok(merged)
            }
            (Operator::Add, Value::String(a), b) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add, a, Value::String(b)) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add | Operator::Sub | Operator::Mul, _, _) => {
                eval_numeric_arithmetic(op, range, &l, left.range, &r, right.range)
            }
            (Operator::Div | Operator::Mod, _, _) => {
                eval_numeric_division(op, range, &l, left.range, &r, right.range)
            }
            (Operator::Eq, a, b) => {
                // Phase C operator lowering: a branded value with a
                // user-defined `eq` method dispatches through it.
                // Falls through to structural Bool(a == b) when no
                // method is registered (so primitives + plain dicts
                // keep current semantics).
                if let Some(out) = self.try_compare_op_method(a, b, "eq", scope, range)? {
                    return Ok(out);
                }
                Ok(Value::Bool(a == b))
            }
            (Operator::Ne, a, b) => {
                if let Some(out) = self.try_compare_op_method(a, b, "eq", scope, range)? {
                    return Ok(invert_bool(out));
                }
                Ok(Value::Bool(a != b))
            }
            (Operator::Lt, a, b) => {
                if let Some(out) = self.try_compare_op_method(a, b, "lt", scope, range)? {
                    return Ok(out);
                }
                eval_numeric_comparison(op, &l, left.range, &r, right.range)
            }
            (Operator::Gt, a, b) => {
                // `a > b` ≡ `b.lt(a)` so a single `lt` witness covers both directions.
                if let Some(out) = self.try_compare_op_method(b, a, "lt", scope, range)? {
                    return Ok(out);
                }
                eval_numeric_comparison(op, &l, left.range, &r, right.range)
            }
            (Operator::Le | Operator::Ge, _, _) => {
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
        range: TokenRange,
        node: &Node,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        let val = self.eval(node, scope)?;
        match (op, val) {
            (Operator::Not, v) => Ok(Value::Bool(!v.is_truthy())),
            (Operator::Sub, Value::Int(i)) => i
                .checked_neg()
                .map(Value::Int)
                .ok_or(RuntimeError::NumericOverflow(range)),
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

    /// Phase C operator lowering: dispatch a comparison op (`==`, `!=`,
    /// `<`, `>`) through a user-defined witness method (`eq`, `lt`)
    /// when the receiver is a branded value whose schema declares one.
    /// Returns `Ok(None)` when no witness applies, so the caller falls
    /// through to the structural / numeric default.
    fn try_compare_op_method(
        &self,
        receiver: &Value,
        other: &Value,
        method_name: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Option<Value>, RuntimeError> {
        let Value::Dict(d) = receiver else {
            return Ok(None);
        };
        let Some(brand) = d.brand.as_ref() else {
            return Ok(None);
        };
        let Some(analyzed) = self.context.analyzed.as_ref() else {
            return Ok(None);
        };
        let Some(method) = analyzed
            .schema_methods
            .get(brand)
            .and_then(|methods| methods.iter().find(|m| m.name == method_name))
        else {
            return Ok(None);
        };
        let Some(body) = method.body_node.as_ref() else {
            return Ok(None);
        };
        let arg = crate::native_fn::EvaluatedArg::positional(other.clone());
        self.invoke_method_body(
            body,
            Some(receiver.clone()),
            &method.params,
            &[arg],
            scope,
            range,
        )
        .map(Some)
    }
}

/// Helper for `Operator::Ne` lowering: invert the truthiness of a
/// `Bool` returned by an `eq` witness call. Non-Bool returns from a
/// user `eq` are unusual — propagate verbatim and let the caller
/// surface an error if the surrounding context expects a Bool.
fn invert_bool(v: Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        other => other,
    }
}

fn eval_numeric_arithmetic(
    op: Operator,
    range: TokenRange,
    left: &Value,
    left_range: TokenRange,
    right: &Value,
    right_range: TokenRange,
) -> Result<Value, RuntimeError> {
    let left = expect_number(left, left_range)?;
    let right = expect_number(right, right_range)?;
    match (op, left, right) {
        (Operator::Add, NumericValue::Int(a), NumericValue::Int(b)) => a
            .checked_add(b)
            .map(Value::Int)
            .ok_or(RuntimeError::NumericOverflow(range)),
        (Operator::Sub, NumericValue::Int(a), NumericValue::Int(b)) => a
            .checked_sub(b)
            .map(Value::Int)
            .ok_or(RuntimeError::NumericOverflow(range)),
        (Operator::Mul, NumericValue::Int(a), NumericValue::Int(b)) => a
            .checked_mul(b)
            .map(Value::Int)
            .ok_or(RuntimeError::NumericOverflow(range)),
        (Operator::Add, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() + b.as_f64()))),
        (Operator::Sub, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() - b.as_f64()))),
        (Operator::Mul, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() * b.as_f64()))),
        _ => unreachable!("non-arithmetic operator passed to eval_numeric_arithmetic"),
    }
}

fn eval_numeric_division(
    op: Operator,
    range: TokenRange,
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
        (Operator::Div, NumericValue::Int(a), NumericValue::Int(b)) => a
            .checked_div(b)
            .map(Value::Int)
            .ok_or(RuntimeError::NumericOverflow(range)),
        (Operator::Mod, NumericValue::Int(a), NumericValue::Int(b)) => a
            .checked_rem(b)
            .map(Value::Int)
            .ok_or(RuntimeError::NumericOverflow(range)),
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
