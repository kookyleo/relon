//! Binary and unary operator evaluation.
//!
//! Covers the arithmetic, comparison, and structural-merge operators that
//! reach `eval_internal` via [`relon_parser::Expr::Binary`] / `Expr::Unary`.
//! The short-circuiting `&&` / `||` and the `|` pipe live in `eval.rs`
//! because their evaluation order interacts with the dispatcher.

use crate::error::RuntimeError;
use crate::eval::TreeWalkEvaluator;
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

impl TreeWalkEvaluator {
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
            && matches!(&l, Value::Schema(_))
            && matches!(
                right.expr.as_ref(),
                Expr::Dict(_) | Expr::Binary(Operator::Add, _, _)
            )
        {
            let Value::Schema(base_box) = l else {
                unreachable!()
            };
            let base_fields = base_box.fields;
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
            return Ok(Value::Schema(Box::new(crate::value::SchemaData {
                generics: Vec::new(),
                fields: merged_fields,
            })));
        }

        let r = self.eval(right, scope)?;
        match (op, &l, &r) {
            (Operator::Add, Value::Dict(_), Value::Dict(_)) => {
                // Phase C.7: branded receiver with an `add` witness wins
                // over structural merge. Without this check `Money + Money`
                // would always merge field-by-field instead of dispatching
                // through the user-defined Addable::add. The witness path
                // takes a body or a host-registered native impl; if neither
                // is present we fall through to the original merge
                // semantics (which keep Logic-as-Data's "two dicts compose
                // structurally" promise).
                if let Some(out) = self.try_arith_op_method(&l, &r, "add", scope, range)? {
                    return Ok(out);
                }
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
            // String concat hot path. The `format!` route through `&str` /
            // `Display` keeps the existing semantics, then we hand the
            // resulting `String` to `SmolStr::from` — payloads ≤
            // `SMOL_STR_INLINE_CAP` (22 B) stay inline and the heap
            // `String` allocation is freed immediately after the inline
            // copy, eliminating the long-lived alloc on short concat
            // intermediates (W3 / W4-style workloads).
            (Operator::Add, Value::String(a), b) => {
                Ok(Value::String(format!("{}{}", a.as_str(), b).into()))
            }
            (Operator::Add, a, Value::String(b)) => {
                Ok(Value::String(format!("{}{}", a, b.as_str()).into()))
            }
            (Operator::Add | Operator::Sub | Operator::Mul, a, b) => {
                // Phase C operator lowering: a branded value whose
                // schema derives Addable / Subtractable / Multiplicable
                // dispatches `+` / `-` / `*` through its witness method
                // (`add` / `sub` / `mul`). Inserted *after* the schema-
                // merge / dict-merge / String-concat short-circuits
                // above and *before* the numeric fallback, so primitive
                // arithmetic on Int / Float keeps current semantics.
                let method = arith_method_for(op);
                if let Some(out) = self.try_arith_op_method(a, b, method, scope, range)? {
                    return Ok(out);
                }
                eval_numeric_arithmetic(op, range, &l, left.range, &r, right.range)
            }
            (Operator::Div | Operator::Mod, a, b) => {
                // Same lowering shape as Add/Sub/Mul. `/` lowers through
                // Divisible::div, `%` through Modable::rem. Numeric
                // fallback handles primitive Int / Float arithmetic.
                let method = arith_method_for(op);
                if let Some(out) = self.try_arith_op_method(a, b, method, scope, range)? {
                    return Ok(out);
                }
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
            (Operator::Le, a, b) => {
                // `a <= b` ≡ `a.lt(b) || a.eq(b)`. We try both witnesses
                // and only commit to the method path when *both* hit —
                // otherwise the operator is ambiguous (one side schema-
                // rooted, the other numeric) and we fall back entirely.
                if let Some(combined) = self.try_le_ge_lowering(a, b, scope, range, false)? {
                    return Ok(combined);
                }
                eval_numeric_comparison(op, &l, left.range, &r, right.range)
            }
            (Operator::Ge, a, b) => {
                // `a >= b` ≡ `b.lt(a) || a.eq(b)`. The `b.lt(a)` half
                // gives strict-greater; the `eq` half closes the
                // boundary. Same all-or-nothing rule as `<=`.
                if let Some(combined) = self.try_le_ge_lowering(a, b, scope, range, true)? {
                    return Ok(combined);
                }
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
    ///
    /// Dispatch precedence:
    /// 1. User-written `.relon` body (`body_node = Some`).
    /// 2. Host-registered native method (`#native`).
    /// 3. None — caller fallback (Phase C.4 auto-derive flows here:
    ///    the analyzer synthesizes a `(eq | to_json)` placeholder
    ///    method with `is_native = true` and no body / native impl,
    ///    so the caller's default semantics take over).
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
        if let Some(body) = method.body_node.as_ref() {
            let arg = crate::native_fn::EvaluatedArg::positional(other.clone());
            return self
                .invoke_method_body(
                    body,
                    Some(receiver.clone()),
                    &method.params,
                    &[arg],
                    scope,
                    range,
                )
                .map(Some);
        }
        // No body — either `#native` (host-implemented) or auto-derived.
        // Try the host registry first; if absent, fall through so the
        // caller's structural default kicks in.
        let key = (brand.clone(), method_name.to_string());
        if let Some(entry) = self.context.native_methods.get(&key) {
            let display_name = format!("{}.{}", brand, method_name);
            self.check_native_fn_capability(&display_name, entry, range)?;
            let mut native = crate::native_fn::NativeArgs::from_evaluated(
                vec![
                    crate::native_fn::EvaluatedArg::positional(receiver.clone()),
                    crate::native_fn::EvaluatedArg::positional(other.clone()),
                ],
                self.caps(),
            );
            // The host fn convention prepends `self` as the first
            // positional arg (see `try_call_native_method` in eval.rs).
            // The two `positional` pushes above already match that
            // shape, so no rotation needed.
            let _ = &mut native; // silence unused-mut lint on future edits
            return Ok(Some(entry.func.call(native, range)?));
        }
        Ok(None)
    }

    /// Combined `<=` / `>=` lowering: synthesize the operator from
    /// `lt` + `eq` witnesses. Returns:
    ///
    /// * `Ok(Some(Bool))` — at least the `lt` witness dispatched. We
    ///   fold in either the `eq` witness's result (when also present)
    ///   or the structural `Value::PartialEq` (Phase C.4 auto-derived
    ///   `eq` fallback) to close the boundary.
    /// * `Ok(None)` — no `lt` witness available. The caller falls back
    ///   to numeric comparison; without `lt` there is no sensible
    ///   schema-rooted answer.
    /// * `Err(_)` — the dispatch reached a witness body but evaluating
    ///   it failed.
    ///
    /// Asymmetry rationale: `lt` is the strict-order discriminator; we
    /// refuse to make up a `<=` / `>=` answer without it. `eq`, in
    /// contrast, has a meaningful structural default (PartialEq on the
    /// dict's contents) thanks to Phase C.4 auto-derive, so we can
    /// close the boundary even when the schema didn't write an
    /// explicit `eq` method.
    ///
    /// `swap_lt = false` for `<=` (uses `a.lt(b)`); `swap_lt = true`
    /// for `>=` (uses `b.lt(a)`).
    fn try_le_ge_lowering(
        &self,
        a: &Value,
        b: &Value,
        scope: &Arc<Scope>,
        range: TokenRange,
        swap_lt: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        let (lt_recv, lt_other) = if swap_lt { (b, a) } else { (a, b) };
        let Some(lt_val) = self.try_compare_op_method(lt_recv, lt_other, "lt", scope, range)?
        else {
            return Ok(None);
        };
        let eq_bool = match self.try_compare_op_method(a, b, "eq", scope, range)? {
            Some(Value::Bool(b)) => b,
            Some(_other) => false,
            None => a == b,
        };
        let lt_bool = matches!(lt_val, Value::Bool(true));
        Ok(Some(Value::Bool(lt_bool || eq_bool)))
    }

    /// Phase C operator lowering: dispatch an arithmetic op (`+`, `-`,
    /// `*`, `/`, `%`) through a user-defined witness method (`add`,
    /// `sub`, `mul`, `div`, `rem`) when the receiver is a branded value
    /// whose schema derives the matching constraint (Addable,
    /// Subtractable, Multiplicable, Divisible, Modable — decision 24).
    /// Returns `Ok(None)` when no witness applies, so the caller falls
    /// through to the numeric default (`eval_numeric_arithmetic` /
    /// `eval_numeric_division`).
    ///
    /// Dispatch precedence mirrors `try_compare_op_method`:
    /// 1. User-written `.relon` body (`body_node = Some`).
    /// 2. Host-registered native method (`#native`).
    /// 3. None — caller fallback (no auto-derive: the arithmetic
    ///    constraints are opt-in, with no synthesized placeholder).
    fn try_arith_op_method(
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
        if let Some(body) = method.body_node.as_ref() {
            let arg = crate::native_fn::EvaluatedArg::positional(other.clone());
            return self
                .invoke_method_body(
                    body,
                    Some(receiver.clone()),
                    &method.params,
                    &[arg],
                    scope,
                    range,
                )
                .map(Some);
        }
        let key = (brand.clone(), method_name.to_string());
        if let Some(entry) = self.context.native_methods.get(&key) {
            let display_name = format!("{}.{}", brand, method_name);
            self.check_native_fn_capability(&display_name, entry, range)?;
            let native = crate::native_fn::NativeArgs::from_evaluated(
                vec![
                    crate::native_fn::EvaluatedArg::positional(receiver.clone()),
                    crate::native_fn::EvaluatedArg::positional(other.clone()),
                ],
                self.caps(),
            );
            return Ok(Some(entry.func.call(native, range)?));
        }
        Ok(None)
    }

    /// Like `try_compare_op_method` but for the zero-arg
    /// `to_json() -> String` witness — used by JsonProjectable
    /// fallback at sites that need a serialized projection (Phase D
    /// follow-up; defined here so the constraint registry stays
    /// colocated with its operator hooks).
    #[allow(dead_code)]
    fn try_to_json_method(
        &self,
        receiver: &Value,
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
            .and_then(|methods| methods.iter().find(|m| m.name == "to_json"))
        else {
            return Ok(None);
        };
        if let Some(body) = method.body_node.as_ref() {
            return self
                .invoke_method_body(
                    body,
                    Some(receiver.clone()),
                    &method.params,
                    &[],
                    scope,
                    range,
                )
                .map(Some);
        }
        let key = (brand.clone(), "to_json".to_string());
        if let Some(entry) = self.context.native_methods.get(&key) {
            let display_name = format!("{}.to_json", brand);
            self.check_native_fn_capability(&display_name, entry, range)?;
            let native = crate::native_fn::NativeArgs::from_evaluated(
                vec![crate::native_fn::EvaluatedArg::positional(receiver.clone())],
                self.caps(),
            );
            return Ok(Some(entry.func.call(native, range)?));
        }
        // Auto-derive fallback: serialize the dict via serde_json.
        match serde_json::to_string(receiver) {
            Ok(s) => Ok(Some(Value::String(s.into()))),
            Err(_) => Ok(None),
        }
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

/// Map an arithmetic operator to the witness method name registered
/// in the constraint table (`crates/relon-analyzer/src/constraints.rs`).
/// `unreachable!` for non-arithmetic operators because the caller in
/// `eval_binary` only invokes this helper from the Add/Sub/Mul/Div/Mod
/// arms.
fn arith_method_for(op: Operator) -> &'static str {
    match op {
        Operator::Add => "add",
        Operator::Sub => "sub",
        Operator::Mul => "mul",
        Operator::Div => "div",
        Operator::Mod => "rem",
        _ => unreachable!("arith_method_for called with non-arithmetic operator"),
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
