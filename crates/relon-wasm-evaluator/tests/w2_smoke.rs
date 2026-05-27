//! W2 smoke: `list.sum(range(n).map((i) => (i + 1) * (i + 2)))` lowered
//! to WASM matches `sum_{i in [0..n)} (i+1)*(i+2)`.
//!
//! Uses the byte-identical production source from
//! `crates/relon-bench/benches/cmp_lua.rs::w2_relon_src()`. The 3
//! honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    `w2_relon_src()`. The lowered loop performs the per-iter
//!    `(i+1) * (i+2)` arithmetic exactly (one mul + two adds), no
//!    closed-form `n*(n+1)*(n+2)/3` substitution. A future drift
//!    would require updating both sites; that's the cost of the
//!    freeze.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is `Value::Int(_)`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W2_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Int\n\
                       list.sum(range(n).map((i) => (i + 1) * (i + 2)))";

/// Tree-walker reference. Computes the same per-iter expression so a
/// regression in the lowered loop (e.g. swapped add/mul ordering)
/// shows up as a mismatch rather than a silently-correct closed-form.
fn expected_w2(n: i64) -> i64 {
    let mut s: i64 = 0;
    for i in 0..n {
        s += (i + 1) * (i + 2);
    }
    s
}

#[test]
fn w2_handles_zero_n() {
    let ev = WasmEvaluator::new(W2_SRC).expect("WasmEvaluator::new(W2)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W2, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w2_matches_tree_walker_small() {
    let ev = WasmEvaluator::new(W2_SRC).expect("WasmEvaluator::new(W2)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let out = ev.run_main(args).expect("run_main(W2, n=10)");
    assert_eq!(out, Value::Int(expected_w2(10)));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w2_matches_tree_walker_at_bench_n() {
    // Bench uses W2_N = 1_000 — drive the same point so the smoke
    // pins the bench's expected value end-to-end.
    let ev = WasmEvaluator::new(W2_SRC).expect("WasmEvaluator::new(W2)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(1_000));
    let out = ev.run_main(args).expect("run_main(W2, n=1000)");
    assert_eq!(out, Value::Int(expected_w2(1_000)));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w2_fast_path_round_trips() {
    // The fast path (`run_main_legacy_i64_fast`) shares the same
    // typed-func handle the bench's `relon_wasm_wasmtime_fast` row
    // calls. Cross-check it agrees with the HashMap-packed path.
    let ev = WasmEvaluator::new(W2_SRC).expect("WasmEvaluator::new(W2)");
    assert!(ev.has_fast_path(), "W2 must expose fast-path entry");
    let fast = ev
        .run_main_legacy_i64_fast(&[1_000])
        .expect("fast(W2, n=1000)");
    assert_eq!(fast, expected_w2(1_000));
}
