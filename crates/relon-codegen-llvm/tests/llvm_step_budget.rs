//! LLVM buffer-entry step budget guard.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Value};

const SRC: &str = "#import list from \"std/list\"\n\
                  #main(Int n) -> Int\n\
                  list.sum(range(n))";

fn args(n: i64) -> HashMap<String, Value> {
    HashMap::from([("n".to_string(), Value::Int(n))])
}

#[test]
fn default_unlimited_budget_keeps_value_path() {
    let ev = LlvmAotEvaluator::from_source(SRC).expect("LLVM from_source");
    let got = ev.run_main(args(10)).expect("LLVM run_main");
    assert_eq!(got, Value::Int(45));
}

#[test]
fn zero_budget_lifts_to_step_limit_exceeded() {
    let ev = LlvmAotEvaluator::from_source(SRC).expect("LLVM from_source");
    ev.set_step_budget(Some(0));
    let err = ev.run_main(args(10)).expect_err("zero budget must trap");
    assert!(
        matches!(err, RuntimeError::StepLimitExceeded { .. }),
        "expected StepLimitExceeded, got {err:?}"
    );
}

#[test]
fn ir_dump_contains_step_budget_guard() {
    let ev = LlvmAotEvaluator::from_source(SRC).expect("LLVM from_source");
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("step_budget"),
        "LLVM IR dump missing step-budget guard:\n{dump}"
    );
}
