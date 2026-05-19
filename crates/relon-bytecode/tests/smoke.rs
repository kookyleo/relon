//! End-to-end smoke for [`BytecodeEvaluator`]: drive simple sources
//! through the full pipeline and check the returned `Value` matches
//! the tree-walker on a few representative shapes.

use std::collections::HashMap;

use relon_bytecode::{BcVmConfig, BytecodeError, BytecodeEvaluator};
use relon_eval_api::{Evaluator, RuntimeError, Value};

fn args_xy(x: i64, y: i64) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Int(x));
    m.insert("y".to_string(), Value::Int(y));
    m
}

#[test]
fn run_main_add() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y").unwrap();
    let v = ev.run_main(args_xy(40, 2)).unwrap();
    assert_eq!(v, Value::Int(42));
}

#[test]
fn run_main_sub() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx - y").unwrap();
    let v = ev.run_main(args_xy(10, 7)).unwrap();
    assert_eq!(v, Value::Int(3));
}

#[test]
fn run_main_mul() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx * y").unwrap();
    let v = ev.run_main(args_xy(6, 7)).unwrap();
    assert_eq!(v, Value::Int(42));
}

#[test]
fn run_main_div() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx / y").unwrap();
    let v = ev.run_main(args_xy(17, 5)).unwrap();
    assert_eq!(v, Value::Int(3));
}

#[test]
fn run_main_div_by_zero_traps() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx / y").unwrap();
    let err = ev.run_main(args_xy(5, 0)).unwrap_err();
    assert!(matches!(err, RuntimeError::DivisionByZero(_)), "{err:?}");
}

#[test]
fn run_main_mod_by_zero_traps() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx % y").unwrap();
    let err = ev.run_main(args_xy(5, 0)).unwrap_err();
    assert!(matches!(err, RuntimeError::DivisionByZero(_)), "{err:?}");
}

#[test]
fn run_main_overflow_traps() {
    let ev = BytecodeEvaluator::from_source("#main(Int x) -> Int\nx + 1").unwrap();
    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Int(i64::MAX));
    let err = ev.run_main(m).unwrap_err();
    assert!(matches!(err, RuntimeError::NumericOverflow(_)), "{err:?}");
}

#[test]
fn run_main_if_expression() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx > y ? x : y").unwrap();
    assert_eq!(ev.run_main(args_xy(17, 5)).unwrap(), Value::Int(17));
    assert_eq!(ev.run_main(args_xy(3, 9)).unwrap(), Value::Int(9));
}

#[test]
fn run_main_cmp() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Bool\nx == y").unwrap();
    assert_eq!(ev.run_main(args_xy(5, 5)).unwrap(), Value::Bool(true));
    assert_eq!(ev.run_main(args_xy(5, 4)).unwrap(), Value::Bool(false));
}

#[test]
fn run_main_let_then_add() {
    let ev =
        BytecodeEvaluator::from_source("#main(Int x) -> Int\n(y + 1) where { y: x * 2 }").unwrap();
    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Int(20));
    assert_eq!(ev.run_main(m).unwrap(), Value::Int(41));
}

#[test]
fn unsupported_source_returns_error() {
    // Stdlib calls aren't yet on the bytecode VM's surface; the
    // compile pass surfaces `Unsupported` rather than silently
    // accepting + producing garbage.
    let err = BytecodeEvaluator::from_source("#main(Int x) -> Int\nabs(x)").unwrap_err();
    assert!(
        matches!(
            err,
            BytecodeError::Compile(_) | BytecodeError::UnsupportedEntry { .. }
        ),
        "{err:?}"
    );
}

#[test]
fn step_limit_trips_resource_prong() {
    // Tight max_steps on a never-fail program: the VM has to fire
    // `WasmStepLimitExceeded` before it returns.
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .unwrap()
        .with_config(BcVmConfig {
            max_steps: Some(1),
            ..BcVmConfig::default()
        });
    let err = ev.run_main(args_xy(1, 2)).unwrap_err();
    assert!(
        matches!(err, RuntimeError::WasmStepLimitExceeded { .. }),
        "{err:?}"
    );
}
