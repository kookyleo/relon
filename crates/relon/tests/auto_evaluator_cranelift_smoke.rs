//! AutoEvaluator + cranelift-AOT integration smoke.
//!
//! v5-β-2 stage 4: wasm-AOT retired here. The auto-tier wrapper now
//! routes `run_main` through cranelift-AOT exclusively; this file
//! locks down the routing contract for the cranelift path.

#![cfg(feature = "cranelift-aot")]

use std::collections::HashMap;

use relon::{new_evaluator, Backend};
use relon_eval_api::Value;

#[test]
fn auto_backend_runs_simple_arith_through_cranelift() {
    // The cranelift backend handles this shape directly after v5-β-2.
    let src = "#main(Int n) -> Int\nn + 1";
    let evaluator = new_evaluator(src, Backend::Auto).expect("Auto backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(41));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn cranelift_backend_runs_simple_arith_directly() {
    // Backend::CraneliftAot bypasses the auto-tier fallback. v5-β-2
    // wired buffer-protocol IR through the cranelift codegen, so this
    // returns a working evaluator and `run_main` produces the same
    // answer as the tree-walker.
    let src = "#main(Int n) -> Int\nn + 1";
    let evaluator = new_evaluator(src, Backend::CraneliftAot).expect("CraneliftAot backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(41));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}
