//! W6 smoke: `list.sum(range(n).map((i) => i + 1))` lowered to WASM
//! matches `n*(n+1)/2`.
//!
//! Source duplicated verbatim from
//! `crates/relon-bench/benches/cmp_lua.rs::w6_relon_src()`. Honesty
//! questions answered in `w1_smoke.rs`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W6_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Int\n\
                       list.sum(range(n).map((i) => i + 1))";
const W6_N: i64 = 10_000;

#[test]
fn w6_list_sum_plus_one_matches_expected() {
    let ev = WasmEvaluator::new(W6_SRC).expect("WasmEvaluator::new(W6)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(W6_N));
    let out = ev.run_main(args).expect("run_main(W6, n=10_000)");
    let expected = W6_N * (W6_N + 1) / 2;
    assert_eq!(out, Value::Int(expected));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w6_handles_zero_n() {
    let ev = WasmEvaluator::new(W6_SRC).expect("WasmEvaluator::new(W6)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W6, n=0)");
    assert_eq!(out, Value::Int(0));
}
