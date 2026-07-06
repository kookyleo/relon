//! `i64::MIN / -1` and `i64::MIN % -1` must lift to the typed
//! `RuntimeError::NumericOverflow`, matching the tree-walk oracle's
//! `checked_div` / `checked_rem` semantics.
//!
//! Regression guard: an unguarded cranelift `sdiv` kills the host
//! process with SIGFPE for this operand pair, and `srem` silently
//! returns `0` — both are backend divergences, not typed traps.

use std::collections::HashMap;

use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};

fn args2(x: i64, y: i64) -> HashMap<String, Value> {
    HashMap::from([
        ("x".to_string(), Value::Int(x)),
        ("y".to_string(), Value::Int(y)),
    ])
}

fn assert_overflow(err: RuntimeError) {
    assert!(
        matches!(err, RuntimeError::NumericOverflow(_)),
        "expected NumericOverflow, got {err:?}"
    );
}

#[test]
fn div_min_by_neg_one_traps_numeric_overflow() {
    let ev = AotEvaluator::from_source("#main(Int x, Int y) -> Int\nx / y").expect("compile");
    assert_overflow(
        ev.run_main(args2(i64::MIN, -1))
            .expect_err("division overflow must trap"),
    );
    // Guard must not fire when only one operand matches the pair.
    assert_eq!(
        ev.run_main(args2(i64::MIN, 1)).expect("min / 1"),
        Value::Int(i64::MIN)
    );
    assert_eq!(
        ev.run_main(args2(-1, i64::MIN)).expect("-1 / min"),
        Value::Int(0)
    );
}

#[test]
fn mod_min_by_neg_one_traps_numeric_overflow() {
    let ev = AotEvaluator::from_source("#main(Int x, Int y) -> Int\nx % y").expect("compile");
    assert_overflow(
        ev.run_main(args2(i64::MIN, -1))
            .expect_err("remainder overflow must trap"),
    );
    assert_eq!(
        ev.run_main(args2(i64::MIN, 1)).expect("min % 1"),
        Value::Int(0)
    );
    assert_eq!(
        ev.run_main(args2(-1, i64::MIN)).expect("-1 % min"),
        Value::Int(-1)
    );
}
