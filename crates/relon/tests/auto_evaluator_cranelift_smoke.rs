//! AutoEvaluator + cranelift-AOT integration smoke.
//!
//! Verifies that `Backend::Auto` produces a working evaluator when
//! the `cranelift-aot` feature is on alongside `wasm-aot`. Since
//! v5-β-2 wired the buffer-protocol entry shape, the cranelift path
//! handles the simple-arithmetic envelope directly; the auto-tier
//! wrapper still falls back to wasm-AOT for richer surfaces.

#![cfg(all(feature = "wasm-aot", feature = "cranelift-aot"))]

use std::collections::HashMap;

use relon::{new_evaluator, Backend};
use relon_eval_api::Value;

#[test]
fn auto_backend_runs_simple_arith_through_cranelift_or_wasm() {
    // The cranelift backend handles this shape directly after v5-β-2;
    // either backend producing the right answer is acceptable here —
    // the auto-tier wrapper picks the cheapest viable one.
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
    // answer as the wasm-AOT reference.
    let src = "#main(Int n) -> Int\nn + 1";
    let evaluator = new_evaluator(src, Backend::CraneliftAot).expect("CraneliftAot backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(41));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn wasm_aot_backend_still_works_when_cranelift_feature_is_on() {
    let src = "#main(Int n) -> Int\nn * 2";
    let evaluator = new_evaluator(src, Backend::WasmAot).expect("WasmAot backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(21));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}
