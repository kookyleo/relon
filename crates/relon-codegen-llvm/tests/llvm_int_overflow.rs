//! LLVM checked Int arithmetic parity.
//!
//! Source-level `run_main` must surface Relon's typed
//! `RuntimeError::NumericOverflow` instead of wrapping or reaching
//! LLVM UB / host traps. The typed fast entry has no error channel, so
//! these tests also pin that public `run_main` routes trap-capable
//! bodies through the buffer entry even when a fast entry was emitted.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
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

fn assert_value(src: &str, x: i64, y: i64, want: i64) {
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM from_source");
    let got = ev.run_main(args2(x, y)).expect("LLVM run_main");
    assert_eq!(got, Value::Int(want));
}

#[test]
fn add_overflow_lifts_to_runtime_error() {
    let src = "#main(Int x, Int y) -> Int\nx + y";
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM from_source");
    assert!(ev.has_fast_path(), "shape still emits explicit fast entry");
    assert_overflow(
        ev.run_main(args2(i64::MAX, 1))
            .expect_err("overflow must trap"),
    );
    assert_value(src, 40, 2, 42);
}

#[test]
fn sub_overflow_lifts_to_runtime_error() {
    let src = "#main(Int x, Int y) -> Int\nx - y";
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM from_source");
    assert_overflow(
        ev.run_main(args2(i64::MIN, 1))
            .expect_err("overflow must trap"),
    );
    assert_value(src, 40, 2, 38);
}

#[test]
fn mul_overflow_lifts_to_runtime_error() {
    let src = "#main(Int x, Int y) -> Int\nx * y";
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM from_source");
    assert_overflow(
        ev.run_main(args2(i64::MAX, 2))
            .expect_err("overflow must trap"),
    );
    assert_value(src, 6, 7, 42);
}

#[test]
fn div_and_mod_min_by_minus_one_lift_to_runtime_error() {
    let div = LlvmAotEvaluator::from_source("#main(Int x, Int y) -> Int\nx / y")
        .expect("LLVM div from_source");
    assert_overflow(
        div.run_main(args2(i64::MIN, -1))
            .expect_err("division overflow must trap"),
    );
    assert_eq!(
        div.run_main(args2(21, -2)).expect("division value"),
        Value::Int(-10)
    );

    let rem = LlvmAotEvaluator::from_source("#main(Int x, Int y) -> Int\nx % y")
        .expect("LLVM mod from_source");
    assert_overflow(
        rem.run_main(args2(i64::MIN, -1))
            .expect_err("remainder overflow must trap"),
    );
    assert_eq!(
        rem.run_main(args2(20, 7)).expect("remainder value"),
        Value::Int(6)
    );
}
