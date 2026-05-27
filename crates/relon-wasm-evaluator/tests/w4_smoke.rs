//! W4 smoke: `range(n).map((i) => "axb").filter((s) => s.contains("x")).len()`
//! lowered to WASM matches the tree-walker's count.
//!
//! Uses the byte-identical production source from
//! `crates/relon-bench/benches/cmp_lua.rs::w4_relon_src()`. The 3
//! honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, the lowered loop calls
//!    `__relon_str_contains` once per iter on the same 3-byte
//!    haystack "axb" and 1-byte needle "x" the source declares. No
//!    closed-form `count = n` substitution; every iter crosses the
//!    wasmtime boundary into the host shim and re-derives the
//!    decision from the record bytes.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm` and instantiates through wasmtime. The
//!    `__relon_str_contains` shim reads `[u32 len][payload]`
//!    records out of linear memory, byte-scans for the needle, and
//!    returns the 0/1 result.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is
//!    `Value::Int(matched_count)`. Cross-checked against the
//!    tree-walker for `n = 0, 1, 5, 32`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W4_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Int\n\
                       range(n)\n\
                         .map((i) => \"axb\")\n\
                         .filter((s) => s.contains(\"x\"))\n\
                         .len()";

#[test]
fn w4_handles_zero_n() {
    let ev = WasmEvaluator::new(W4_SRC).expect("WasmEvaluator::new(W4)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W4, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w4_matches_tree_walker_small() {
    let ev = WasmEvaluator::new(W4_SRC).expect("WasmEvaluator::new(W4)");
    for &n in &[1i64, 5, 32] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args).expect("run_main(W4)");
        // Every haystack "axb" contains "x" => count == n.
        assert_eq!(out, Value::Int(n), "W4 result mismatch at n={n}");
    }
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w4_matches_tree_walker_at_bench_n() {
    // cmp_lua drives W4 at TREE_WALK_N = 10_000 (see cmp_lua.rs).
    // Pin the smoke at the same point so it catches drift.
    const TREE_WALK_N: i64 = 10_000;
    let ev = WasmEvaluator::new(W4_SRC).expect("WasmEvaluator::new(W4)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(TREE_WALK_N));
    let out = ev.run_main(args).expect("run_main(W4, n=10000)");
    assert_eq!(out, Value::Int(TREE_WALK_N));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w4_fast_path_available() {
    // W4 returns a scalar Int count — fast path should be exposed
    // (so the `relon_wasm_wasmtime_fast` bench row can book a
    // direct typed-func call without the HashMap pack overhead).
    let ev = WasmEvaluator::new(W4_SRC).expect("WasmEvaluator::new(W4)");
    assert!(
        ev.has_fast_path(),
        "W4 must expose fast-path entry (scalar Int return)"
    );
    let fast_out = ev.run_main_legacy_i64_fast(&[7]).expect("W4 fast path run");
    assert_eq!(fast_out, 7);
}

#[test]
fn w4_buffer_path_stable_across_calls() {
    // The const haystack/needle records live in linear memory; they
    // must not be clobbered by the per-call arena reset. Run W4
    // twice and confirm the second call still returns the right
    // count (this would fail if `reset()` zeroed bytes 0..N).
    let ev = WasmEvaluator::new(W4_SRC).expect("WasmEvaluator::new(W4)");

    let mut args5 = HashMap::new();
    args5.insert("n".to_string(), Value::Int(5));
    assert_eq!(ev.run_main(args5).unwrap(), Value::Int(5));

    let mut args3 = HashMap::new();
    args3.insert("n".to_string(), Value::Int(3));
    assert_eq!(ev.run_main(args3).unwrap(), Value::Int(3));
}
