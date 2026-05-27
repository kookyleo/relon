//! W12 smoke: `x + 1` — the trivial happy-path. Source byte-identical
//! to `crates/relon-bench/benches/cmp_lua.rs::w12_relon_src()`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W12_SRC: &str = "#main(Int x) -> Int\nx + 1";

#[test]
fn w12_increment_int_matches_expected() {
    let ev = WasmEvaluator::new(W12_SRC).expect("WasmEvaluator::new(W12)");

    // Cold tier before any call.
    assert_eq!(ev.active_tier(), Tier::Cold);

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(41));
    let out = ev.run_main(args).expect("run_main(W12, x=41)");
    assert_eq!(out, Value::Int(42));

    // Compiled tier after a successful call.
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w12_negative_input() {
    let ev = WasmEvaluator::new(W12_SRC).expect("WasmEvaluator::new(W12)");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(-1));
    let out = ev.run_main(args).expect("run_main(W12, x=-1)");
    assert_eq!(out, Value::Int(0));
}
