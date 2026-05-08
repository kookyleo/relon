//! Constant folding for arithmetic on literals.
//!
//! Walks Binary / Unary nodes whose every leaf is a numeric literal
//! and computes the result with `checked_*` arithmetic. When the fold
//! hits a divide-by-zero or an integer overflow, we emit a static
//! diagnostic instead of letting the same expression re-explode in
//! the evaluator. Floats fold without reporting (IEEE-754 Inf/NaN
//! are valid). Anything containing a non-literal sub-tree (Variable,
//! Reference, FnCall, Closure, Dict, List, FString, Ternary, Match)
//! returns `Ok(None)` — runtime keeps the truth.
//!
//! Stage 5 of the staged hardening roadmap. The fold is intentionally
//! literal-only: bool / string folding is out of scope for v1, and
//! anything that branches on data (`Ternary`, `Match`) is left to the
//! evaluator. Mixed `Int`/`Float` operands promote to `Float` to mirror
//! the evaluator's promotion rule (see `eval_numeric_arithmetic`).

use relon_parser::{Expr, Node, Operator, TokenRange};

/// A numeric literal value that has been statically folded. Bool is
/// included so unary `!` could be supported in a future iteration —
/// today we never produce or consume `Bool` here, but carrying the
/// variant lets `apply_*` exhaustively pattern-match without panicking
/// on shapes the upper walker might one day pass in.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ConstValue {
    Int(i64),
    Float(f64),
    #[allow(dead_code)]
    Bool(bool),
}

/// Fold-time error surfaced as a static analyzer diagnostic. Carries
/// the `TokenRange` of the *outermost* arithmetic node that triggered
/// it so the user-visible label points at the failing expression as a
/// whole, not the literal `0` divisor in isolation.
#[derive(Debug, Clone)]
pub(crate) enum FoldError {
    DivByZero(TokenRange),
    Overflow { op: Operator, range: TokenRange },
}

/// Recursively fold `node` if every leaf is a numeric literal.
///
/// * `Ok(Some(v))` — the entire subtree collapsed to a constant.
/// * `Ok(None)` — at least one sub-expression is non-literal; defer to
///   runtime.
/// * `Err(FoldError)` — the arithmetic itself is statically broken
///   (div-by-zero or i64 overflow).
pub(crate) fn try_fold(node: &Node) -> Result<Option<ConstValue>, FoldError> {
    match node.expr.as_ref() {
        Expr::Int(i) => Ok(Some(ConstValue::Int(*i))),
        Expr::Float(f) => Ok(Some(ConstValue::Float(f.into_inner()))),
        Expr::Bool(b) => Ok(Some(ConstValue::Bool(*b))),
        Expr::Binary(op, l, r) => {
            let lv = try_fold(l)?;
            let rv = try_fold(r)?;
            match (lv, rv) {
                (Some(a), Some(b)) => apply_binary(*op, a, b, node.range),
                _ => Ok(None),
            }
        }
        Expr::Unary(op, inner) => {
            let v = try_fold(inner)?;
            match v {
                Some(a) => apply_unary(*op, a, node.range),
                None => Ok(None),
            }
        }
        // Anything else (Variable, Reference, FnCall, Closure, Dict,
        // List, FString, Ternary, Match, VariantCtor, Type, Wildcard,
        // String, Null, Where, Spread, Comprehension) is non-foldable
        // at this stage. Runtime keeps owning the verdict.
        _ => Ok(None),
    }
}

fn apply_binary(
    op: Operator,
    l: ConstValue,
    r: ConstValue,
    range: TokenRange,
) -> Result<Option<ConstValue>, FoldError> {
    use ConstValue::{Float, Int};
    match (op, l, r) {
        // ---- Int / Int ----
        (Operator::Add, Int(a), Int(b)) => a
            .checked_add(b)
            .map(|v| Some(Int(v)))
            .ok_or(FoldError::Overflow { op, range }),
        (Operator::Sub, Int(a), Int(b)) => a
            .checked_sub(b)
            .map(|v| Some(Int(v)))
            .ok_or(FoldError::Overflow { op, range }),
        (Operator::Mul, Int(a), Int(b)) => a
            .checked_mul(b)
            .map(|v| Some(Int(v)))
            .ok_or(FoldError::Overflow { op, range }),
        // Div / Mod by zero — the headline diagnostic for this stage.
        (Operator::Div, Int(_), Int(0)) => Err(FoldError::DivByZero(range)),
        (Operator::Mod, Int(_), Int(0)) => Err(FoldError::DivByZero(range)),
        (Operator::Div, Int(a), Int(b)) => a
            .checked_div(b)
            .map(|v| Some(Int(v)))
            .ok_or(FoldError::Overflow { op, range }),
        (Operator::Mod, Int(a), Int(b)) => a
            .checked_rem(b)
            .map(|v| Some(Int(v)))
            .ok_or(FoldError::Overflow { op, range }),

        // ---- Float / Float ---- (IEEE-754: never errors)
        (Operator::Add, Float(a), Float(b)) => Ok(Some(Float(a + b))),
        (Operator::Sub, Float(a), Float(b)) => Ok(Some(Float(a - b))),
        (Operator::Mul, Float(a), Float(b)) => Ok(Some(Float(a * b))),
        (Operator::Div, Float(a), Float(b)) => Ok(Some(Float(a / b))),
        (Operator::Mod, Float(a), Float(b)) => Ok(Some(Float(a % b))),

        // ---- Mixed Int / Float — promote to Float (evaluator parity) ----
        (Operator::Add, Int(a), Float(b)) => Ok(Some(Float(a as f64 + b))),
        (Operator::Add, Float(a), Int(b)) => Ok(Some(Float(a + b as f64))),
        (Operator::Sub, Int(a), Float(b)) => Ok(Some(Float(a as f64 - b))),
        (Operator::Sub, Float(a), Int(b)) => Ok(Some(Float(a - b as f64))),
        (Operator::Mul, Int(a), Float(b)) => Ok(Some(Float(a as f64 * b))),
        (Operator::Mul, Float(a), Int(b)) => Ok(Some(Float(a * b as f64))),
        (Operator::Div, Int(a), Float(b)) => Ok(Some(Float(a as f64 / b))),
        (Operator::Div, Float(a), Int(b)) => Ok(Some(Float(a / b as f64))),
        (Operator::Mod, Int(a), Float(b)) => Ok(Some(Float(a as f64 % b))),
        (Operator::Mod, Float(a), Int(b)) => Ok(Some(Float(a % b as f64))),

        // Comparison / logical / pipe / concat and any Bool-involved
        // arithmetic falls through. v1 only diagnoses arithmetic on
        // numeric literals; everything else stays runtime's verdict.
        _ => Ok(None),
    }
}

fn apply_unary(
    op: Operator,
    v: ConstValue,
    range: TokenRange,
) -> Result<Option<ConstValue>, FoldError> {
    use ConstValue::{Float, Int};
    match (op, v) {
        (Operator::Sub, Int(a)) => a
            .checked_neg()
            .map(|v| Some(Int(v)))
            .ok_or(FoldError::Overflow { op, range }),
        (Operator::Sub, Float(a)) => Ok(Some(Float(-a))),
        // `!` on bool / other unary shapes — leave to runtime.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_parser::parse_document;

    fn fold_root_value(src: &str) -> Result<Option<ConstValue>, FoldError> {
        let root = parse_document(src).expect("parse");
        // Tests inspect the field named `x`. Allow auxiliary fields
        // (e.g. local bindings) to precede it without failing the fold.
        match root.expr.as_ref() {
            Expr::Dict(pairs) => {
                for (key, value) in pairs {
                    if let relon_parser::TokenKey::String(name, _, _) = key {
                        if name == "x" {
                            return try_fold(value);
                        }
                    }
                }
                panic!("test fixture must contain a field named `x`");
            }
            _ => panic!("test fixture must be a dict literal"),
        }
    }

    #[test]
    fn folds_simple_int_arithmetic() {
        let v = fold_root_value("{ x: 1 + 2 }").unwrap().unwrap();
        match v {
            ConstValue::Int(3) => {}
            other => panic!("expected Int(3), got {other:?}"),
        }
    }

    #[test]
    fn folds_chained_int_arithmetic() {
        let v = fold_root_value("{ x: (1 + 2) * (3 + 4) }")
            .unwrap()
            .unwrap();
        match v {
            ConstValue::Int(21) => {}
            other => panic!("expected Int(21), got {other:?}"),
        }
    }

    #[test]
    fn reports_div_by_zero() {
        let err = fold_root_value("{ x: 1 / 0 }").unwrap_err();
        assert!(matches!(err, FoldError::DivByZero(_)));
    }

    #[test]
    fn reports_mod_by_zero() {
        let err = fold_root_value("{ x: 100 % 0 }").unwrap_err();
        assert!(matches!(err, FoldError::DivByZero(_)));
    }

    #[test]
    fn reports_overflow_add() {
        let err = fold_root_value("{ x: 9223372036854775807 + 1 }").unwrap_err();
        assert!(matches!(
            err,
            FoldError::Overflow {
                op: Operator::Add,
                ..
            }
        ));
    }

    #[test]
    fn reports_overflow_chained_mul() {
        // 1_000_000 ^ 4 = 1e24 > i64::MAX (~9.2e18) — overflows on the
        // third multiplication, which is the one we want to flag.
        let err = fold_root_value("{ x: 1000000 * 1000000 * 1000000 * 1000000 }").unwrap_err();
        assert!(matches!(
            err,
            FoldError::Overflow {
                op: Operator::Mul,
                ..
            }
        ));
    }

    #[test]
    fn folds_subtree_then_div_zero() {
        let err = fold_root_value("{ x: (1 + 2) * (3 + 4) / 0 }").unwrap_err();
        assert!(matches!(err, FoldError::DivByZero(_)));
    }

    #[test]
    fn float_div_zero_silent() {
        // IEEE-754: 1.0 / 0.0 is +Inf, not an error.
        let v = fold_root_value("{ x: 1.0 / 0.0 }").unwrap().unwrap();
        match v {
            ConstValue::Float(f) => assert!(f.is_infinite()),
            other => panic!("expected Float(inf), got {other:?}"),
        }
    }

    #[test]
    fn variable_in_subtree_returns_none() {
        // The fold pass doesn't consult any scope — any `Variable`
        // head should short-circuit to `None` so runtime keeps the
        // verdict.
        let res = fold_root_value("{ a: 1, x: a + 1 }").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn fn_call_in_subtree_returns_none() {
        let res = fold_root_value("{ x: 1 + len([1, 2, 3]) }").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn ternary_does_not_fold() {
        // Even when both branches would be foldable, we skip Ternary
        // entirely — branches depend on data, runtime owns it.
        let res = fold_root_value("{ x: true ? 1 / 0 : 0 }").unwrap();
        assert!(res.is_none());
    }
}
