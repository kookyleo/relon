//! W1 smoke: `list.sum(range(n))` lowered to WASM matches `n*(n-1)/2`.
//!
//! Uses the byte-identical production source from
//! `crates/relon-bench/benches/cmp_lua.rs::w1_relon_src()`. The 3
//! honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    `w1_relon_src()`. A future drift would require updating both
//!    sites; that's the cost of the freeze.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is `Value::Int(_)`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W1_SRC: &str = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";
const W1_N: i64 = 10_000;

#[test]
fn w1_int_sum_range_matches_expected() {
    let ev = WasmEvaluator::new(W1_SRC).expect("WasmEvaluator::new(W1)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(W1_N));
    let out = ev.run_main(args).expect("run_main(W1, n=10_000)");
    // sum(0..n-1) = n*(n-1)/2
    let expected = W1_N * (W1_N - 1) / 2;
    assert_eq!(out, Value::Int(expected));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w1_handles_zero_n() {
    let ev = WasmEvaluator::new(W1_SRC).expect("WasmEvaluator::new(W1)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W1, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}
