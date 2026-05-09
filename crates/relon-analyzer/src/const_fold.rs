//! Constant folding for arithmetic on literals.
//!
//! Walks Binary / Unary / Ternary nodes whose relevant leaves are
//! literals and computes the result with `checked_*` arithmetic (for
//! integer ops) or direct evaluation (for bool / string / float).
//! When the fold hits a divide-by-zero or an integer overflow, we
//! emit a static diagnostic instead of letting the same expression
//! re-explode in the evaluator. Floats fold without reporting
//! (IEEE-754 Inf/NaN are valid). Anything containing a non-literal
//! sub-tree (Variable, Reference, FnCall, Closure, Dict, List,
//! FString, Match) returns `Ok(None)` — runtime keeps the truth.
//!
//! Stage 5 introduced numeric folding; v1.1 extends coverage to bool
//! short-circuit (`&&` / `||` / `!`), string-literal concatenation
//! (`"a" + "b"`), and ternary-on-bool branch selection (`true ? a :
//! b` → fold `a`). The walker still recurses into children of a
//! non-folding node, so a literal `1 / 0` hidden in an unreachable
//! ternary branch still raises — pruning unreachable branches is the
//! job of the upcoming control-flow pass (#41), not this one.
//! Mixed `Int`/`Float` operands promote to `Float` to mirror the
//! evaluator's promotion rule (see `eval_numeric_arithmetic`).

use relon_parser::{Expr, Node, Operator, TokenRange};

/// A literal value that has been statically folded. Carries `Bool`
/// (consumed by `&&`, `||`, `!`, and ternary branch selection) and
/// `String` (consumed by literal `+` concatenation) alongside the
/// numeric variants. Cloned (not copied) because `String` owns a heap
/// allocation; the fold keeps clones cheap by only constructing them
/// at literal leaves and at the final concat result.
#[derive(Debug, Clone)]
pub(crate) enum ConstValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
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
        // FString carries interpolations and is *not* a literal — only
        // the bare-string variant qualifies. The walker still descends
        // into FString parts via `child_nodes`, so a `1 / 0` hidden in
        // an interpolation slot is reported normally.
        Expr::String(s) => Ok(Some(ConstValue::String(s.clone()))),
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
        // Ternary on a constant `Bool` cond collapses to the chosen
        // branch's fold result. The unchosen branch is intentionally
        // *not* skipped by the caller's child-walk — pruning it is
        // #41's territory; here we only resolve the value when both
        // cond and the chosen branch are foldable.
        Expr::Ternary { cond, then, els } => {
            let cond_val = try_fold(cond)?;
            match cond_val {
                Some(ConstValue::Bool(true)) => try_fold(then),
                Some(ConstValue::Bool(false)) => try_fold(els),
                // Non-bool const cond (e.g. `1 ? a : b`) is a type
                // error the type-checker owns; we just decline to
                // fold and let runtime / typecheck speak.
                Some(_) => Ok(None),
                None => Ok(None),
            }
        }
        // Anything else (Variable, Reference, FnCall, Closure, Dict,
        // List, FString, Match, VariantCtor, Type, Wildcard, Null,
        // Where, Spread, Comprehension) is non-foldable at this stage.
        // Runtime keeps owning the verdict.
        _ => Ok(None),
    }
}

/// `Some(b)` when `cond` folds to a known constant `Bool` (so its
/// truthiness is statically decidable), `None` otherwise. Convenience
/// over [`try_fold`] — strips the [`FoldError`] and non-bool fold
/// results so callers asking *only* "is this branch reachable?" don't
/// have to pattern-match the full result shape.
///
/// Used by reachability-aware passes (capability check) to skip dead
/// branches of `cond ? a : b` and short-circuit `&&` / `||`. A
/// fold-time error in `cond` (`1 / 0 ? a : b`) intentionally resolves
/// to `None` here — the diagnostic already fires through `try_fold`
/// from the type-checker's walker; this helper just declines to prune.
pub(crate) fn const_bool_branch(cond: &Node) -> Option<bool> {
    match try_fold(cond) {
        Ok(Some(ConstValue::Bool(b))) => Some(b),
        _ => None,
    }
}

/// Identify the dead branch of a control-flow node whose decision is
/// statically known. Returns the unreachable child when one exists,
/// `None` when both branches stay live (cond non-constant, or `node`
/// isn't a control-flow shape).
///
/// Recognises:
/// * `Expr::Ternary` — `true ? t : e` → dead is `e`; `false ? t : e`
///   → dead is `t`.
/// * `Expr::Binary(Operator::And, l, r)` — `false && r` → dead is `r`
///   (short-circuit). `true && r` keeps `r` live (its value decides
///   the whole expression).
/// * `Expr::Binary(Operator::Or, l, r)` — `true || r` → dead is `r`.
///   `false || r` keeps `r` live.
///
/// Does *not* recurse — callers walk the tree themselves and call this
/// helper at each node they consider for pruning. The returned
/// reference borrows `node`; the caller compares descendant ids
/// against it (or recurses with `child_nodes`) to mark unreachable
/// sub-trees.
pub(crate) fn dead_branch_of(node: &Node) -> Option<&Node> {
    match node.expr.as_ref() {
        Expr::Ternary { cond, then, els } => match const_bool_branch(cond)? {
            true => Some(els),
            false => Some(then),
        },
        Expr::Binary(Operator::And, _l, r) => {
            if const_bool_branch(_l) == Some(false) {
                Some(r)
            } else {
                None
            }
        }
        Expr::Binary(Operator::Or, _l, r) => {
            if const_bool_branch(_l) == Some(true) {
                Some(r)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn apply_binary(
    op: Operator,
    l: ConstValue,
    r: ConstValue,
    range: TokenRange,
) -> Result<Option<ConstValue>, FoldError> {
    use ConstValue::{Bool, Float, Int, String as Str};
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

        // ---- Bool && / || ---- (no diagnostics; both branches still
        // walked by the caller, so a literal `1 / 0` hidden in the
        // unreached side is still reported via child-walk).
        (Operator::And, Bool(a), Bool(b)) => Ok(Some(Bool(a && b))),
        (Operator::Or, Bool(a), Bool(b)) => Ok(Some(Bool(a || b))),

        // ---- String literal concat via `+` ---- (the parser routes
        // `"a" + "b"` through `Operator::Add`; `Operator::Concat` is
        // unused at parse time, but listed here so a future swap to
        // a dedicated concat operator drops in cleanly).
        (Operator::Add | Operator::Concat, Str(a), Str(b)) => Ok(Some(Str(a + &b))),

        // Comparison / pipe and any cross-type combination
        // (`1 + true`, `"a" + 1`, ...) falls through. v1 only
        // diagnoses arithmetic on numeric literals; everything
        // else stays runtime's verdict.
        _ => Ok(None),
    }
}

fn apply_unary(
    op: Operator,
    v: ConstValue,
    range: TokenRange,
) -> Result<Option<ConstValue>, FoldError> {
    use ConstValue::{Bool, Float, Int};
    match (op, v) {
        (Operator::Sub, Int(a)) => a
            .checked_neg()
            .map(|v| Some(Int(v)))
            .ok_or(FoldError::Overflow { op, range }),
        (Operator::Sub, Float(a)) => Ok(Some(Float(-a))),
        (Operator::Not, Bool(b)) => Ok(Some(Bool(!b))),
        // `Not` on numeric / string and other unary shapes — leave
        // the verdict to runtime / typecheck.
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

    // ----- v1.1 extensions: Bool / String / Ternary -----------------

    #[test]
    fn folds_bool_and() {
        let v = fold_root_value("{ x: true && false }").unwrap().unwrap();
        match v {
            ConstValue::Bool(false) => {}
            other => panic!("expected Bool(false), got {other:?}"),
        }
    }

    #[test]
    fn folds_bool_or() {
        let v = fold_root_value("{ x: false || true }").unwrap().unwrap();
        match v {
            ConstValue::Bool(true) => {}
            other => panic!("expected Bool(true), got {other:?}"),
        }
    }

    #[test]
    fn folds_bool_not() {
        let v = fold_root_value("{ x: !true }").unwrap().unwrap();
        match v {
            ConstValue::Bool(false) => {}
            other => panic!("expected Bool(false), got {other:?}"),
        }
    }

    #[test]
    fn folds_string_concat() {
        let v = fold_root_value(r#"{ x: "hi " + "there" }"#)
            .unwrap()
            .unwrap();
        match v {
            ConstValue::String(s) => assert_eq!(s, "hi there"),
            other => panic!("expected String(\"hi there\"), got {other:?}"),
        }
    }

    #[test]
    fn folds_ternary_true_branch() {
        let v = fold_root_value("{ x: true ? 1 : 2 }").unwrap().unwrap();
        match v {
            ConstValue::Int(1) => {}
            other => panic!("expected Int(1), got {other:?}"),
        }
    }

    #[test]
    fn folds_ternary_false_branch() {
        let v = fold_root_value("{ x: false ? 1 : 2 }").unwrap().unwrap();
        match v {
            ConstValue::Int(2) => {}
            other => panic!("expected Int(2), got {other:?}"),
        }
    }

    #[test]
    fn ternary_with_unreachable_div_zero_still_walked_by_caller() {
        // Stage 5's contract — and v1.1 keeps it: the fold collapses
        // to the chosen branch (`0`), but the unreached `1 / 0` is
        // *not* pruned. Walker child-walk still hits the literal
        // div-by-zero and reports it. Asserting from inside `try_fold`
        // we just see the FoldError bubble from the recursive call
        // into the unreached branch — `try_fold` is recursive and
        // visits both sides of the ternary's children too? No: this
        // implementation only folds the chosen side. So fold-time
        // here returns `Some(Int(0))`, and child-walk reports.
        let v = fold_root_value("{ x: true ? 0 : 1 / 0 }").unwrap().unwrap();
        match v {
            ConstValue::Int(0) => {}
            other => panic!("expected Int(0), got {other:?}"),
        }
    }

    #[test]
    fn variable_short_circuits_bool_and() {
        // `x && true` — `x` is non-literal, fold declines.
        let res = fold_root_value("{ y: true, x: y && true }").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn ternary_with_variable_cond_returns_none() {
        let res = fold_root_value("{ y: true, x: y ? 1 : 2 }").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn string_concat_with_variable_returns_none() {
        let res = fold_root_value(r#"{ y: "b", x: "a" + y }"#).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn cross_type_int_plus_bool_returns_none() {
        // `1 + true` is a type error the type-checker reports; the
        // fold simply declines (no `FoldError`, no constant value).
        let res = fold_root_value("{ x: 1 + true }").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn fstring_does_not_count_as_string_literal() {
        // FString is *not* a literal — fold declines so the walker
        // descends into its parts.
        let res = fold_root_value(r#"{ x: f"hello {1 + 2}" }"#).unwrap();
        assert!(res.is_none());
    }
}
