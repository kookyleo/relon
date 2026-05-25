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
    // Bytecode-coverage-expansion B-2 widened the scalar envelope to
    // also accept `String` args / returns. Pick a construct the
    // envelope still bounces — `List` return types stay outside the
    // scaffold because list ↔ host-`Value::List` marshalling is not
    // yet wired through `pack_args` / `unpack_return_slots`.
    let err = BytecodeEvaluator::from_source(
        "#import list from \"std/list\"\n#main(Int n) -> List<Int>\nrange(n)",
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            BytecodeError::Compile(_)
                | BytecodeError::UnsupportedEntry { .. }
                | BytecodeError::Analyze(_)
                | BytecodeError::Lowering(_)
        ),
        "{err:?}"
    );
}

/// Bytecode-coverage-expansion B-2: confirm the widened envelope
/// accepts `String` args / returns end-to-end through the production
/// `from_source` pipeline — the same shape the deopt-landing path
/// (Phase B-3) uses to drive trace-jit recovery into the bytecode VM.
#[test]
fn string_arg_and_return_round_trip() {
    let ev = BytecodeEvaluator::from_source("#main(String s) -> String\ns + \" world\"")
        .expect("compile");
    let mut m = HashMap::new();
    m.insert(
        "s".to_string(),
        Value::String(relon_eval_api::SmolStr::from("hello")),
    );
    let v = ev.run_main(m).unwrap();
    match v {
        Value::String(s) => assert_eq!(s.as_str(), "hello world"),
        other => panic!("expected Value::String, got {other:?}"),
    }
}

#[test]
fn string_contains_call_round_trip() {
    let ev = BytecodeEvaluator::from_source("#main(String s) -> Bool\ns.contains(\"x\")")
        .expect("compile");
    let mut m_hit = HashMap::new();
    m_hit.insert(
        "s".to_string(),
        Value::String(relon_eval_api::SmolStr::from("axb")),
    );
    assert_eq!(ev.run_main(m_hit).unwrap(), Value::Bool(true));
    let mut m_miss = HashMap::new();
    m_miss.insert(
        "s".to_string(),
        Value::String(relon_eval_api::SmolStr::from("abc")),
    );
    assert_eq!(ev.run_main(m_miss).unwrap(), Value::Bool(false));
}

#[test]
fn string_substring_call_round_trip() {
    let ev = BytecodeEvaluator::from_source("#main(String s) -> String\ns.substring(1, 3)")
        .expect("compile");
    let mut m = HashMap::new();
    m.insert(
        "s".to_string(),
        Value::String(relon_eval_api::SmolStr::from("hello")),
    );
    match ev.run_main(m).unwrap() {
        Value::String(s) => assert_eq!(s.as_str(), "ell"),
        other => panic!("expected Value::String(\"ell\"), got {other:?}"),
    }
}

#[test]
fn run_main_abs_inlined() {
    // v6-δ M2-B widening: the bytecode VM inlines bundled stdlib
    // bodies that fit the arith-control envelope. `abs(x)` resolves
    // to the `abs_int` body which uses `Select`; the bytecode
    // compile pass lowers it via the new `compile_select` helper.
    let ev = BytecodeEvaluator::from_source("#main(Int x) -> Int\nabs(x)").unwrap();
    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Int(-42));
    assert_eq!(ev.run_main(m).unwrap(), Value::Int(42));

    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Int(7));
    assert_eq!(ev.run_main(m).unwrap(), Value::Int(7));
}

#[test]
fn run_main_min_max_inlined() {
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nmin(x, y)").unwrap();
    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Int(17));
    m.insert("y".to_string(), Value::Int(5));
    assert_eq!(ev.run_main(m).unwrap(), Value::Int(5));

    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nmax(x, y)").unwrap();
    let mut m = HashMap::new();
    m.insert("x".to_string(), Value::Int(17));
    m.insert("y".to_string(), Value::Int(5));
    assert_eq!(ev.run_main(m).unwrap(), Value::Int(17));
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
