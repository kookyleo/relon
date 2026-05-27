//! W5 smoke: the inline-Int variant of W5 (matching
//! `w5_relon_src_bytecode()` in `crates/relon-bench/benches/cmp_lua.rs`)
//! lowered to WASM matches the dict-lookup sum
//! `Σ_{i in [0..n)} ((i % 10) + 1)`.
//!
//! The 3 honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    `w5_relon_src_bytecode()`. The bytecode-shape source already
//!    algebraically collapsed the production source's `d[keys[i % 10]]`
//!    chain to `(i % 10) + 1` (the dict's `a..j -> 1..10` mapping is
//!    declaration-ordered, so picking the n-th key picks the n-th
//!    value). The WASM lowering chose to **not** copy that closed
//!    form — emitting `(i % 10) + 1` as a single rem+add would book
//!    the dict-lookup cost as scalar arith (paper-win anti-pattern).
//!    Instead the loop reads `table[i % 10]` from a 10-entry i64
//!    linear-memory data segment per iter, keeping a real memory
//!    dependency that models what the production dict lookup would do.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is `Value::Int(_)`.
//!
//! Note: the **production** W5 source (`#main(Int n) -> Dict` with
//! `#internal d: { ... }` + `keys: [...]` literals and a string-keyed
//! `d[keys[i % 10]]` lookup) still scope-cuts at the classifier and
//! routes through the tree-walker fallback — that path is Z.4
//! follow-up. See `scope_cut_smoke.rs` for the scope-cut tier check.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// Byte-identical to `w5_relon_src_bytecode()` in cmp_lua.
const W5_INLINE_SRC: &str = "#import list from \"std/list\"\n\
                             #main(Int n) -> Int\n\
                             list.sum(range(n).map((i) => (i % 10) + 1))";

/// Tree-walker reference. Computes the same sum the lowered table-load
/// loop emits — a regression in the data-segment payload (e.g. an
/// off-by-one in the `[1..=10]` table or a wrong `idx * 8` scaling)
/// shows up as a mismatch here rather than a silently-correct
/// closed-form.
fn expected_w5(n: i64) -> i64 {
    let mut acc: i64 = 0;
    for i in 0..n {
        acc += (i % 10) + 1;
    }
    acc
}

#[test]
fn w5_handles_zero_n() {
    let ev = WasmEvaluator::new(W5_INLINE_SRC).expect("WasmEvaluator::new(W5 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W5, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w5_handles_each_table_entry_individually() {
    // n=1..=10 visits each table entry exactly once at the trailing
    // iteration. Catches a regression that scrambled the table payload
    // (e.g. wrote `[10, 1, 2, ..., 9]` instead of `[1, 2, ..., 10]`)
    // or used the wrong `idx * 8` byte stride.
    for n in 1..=10 {
        let ev = WasmEvaluator::new(W5_INLINE_SRC).expect("WasmEvaluator::new(W5 inline)");
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev
            .run_main(args)
            .unwrap_or_else(|e| panic!("run_main(W5, n={n}): {e}"));
        assert_eq!(
            out,
            Value::Int(expected_w5(n)),
            "W5 dispatch-table mismatch at n={n} (each iter visits one new table entry)"
        );
        assert_eq!(ev.active_tier(), Tier::Compiled);
    }
}

#[test]
fn w5_matches_tree_walker_small() {
    // n=10 visits each table entry exactly once, summing 1+2+...+10 = 55.
    // Catches a missed wrap (e.g. `i % 10` lowered as `i % 9`).
    let ev = WasmEvaluator::new(W5_INLINE_SRC).expect("WasmEvaluator::new(W5 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let out = ev.run_main(args).expect("run_main(W5, n=10)");
    assert_eq!(out, Value::Int(expected_w5(10)));
    assert_eq!(out, Value::Int(55));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w5_matches_tree_walker_at_bench_n() {
    // Bench uses TREE_WALK_N = 10_000 (see cmp_lua.rs); drive the
    // same point so the smoke pins the bench's expected value end-
    // to-end. The constant is duplicated here intentionally — the
    // smoke crate doesn't depend on the bench fixtures.
    let ev = WasmEvaluator::new(W5_INLINE_SRC).expect("WasmEvaluator::new(W5 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10_000));
    let out = ev.run_main(args).expect("run_main(W5, n=10000)");
    assert_eq!(out, Value::Int(expected_w5(10_000)));
    // n=10_000 = 1_000 full cycles, each summing 55 → 1_000 * 55 = 55_000.
    assert_eq!(out, Value::Int(55_000));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w5_fast_path_round_trips() {
    // The fast path (`run_main_legacy_i64_fast`) shares the same
    // typed-func handle the bench's `relon_wasm_wasmtime_fast` row
    // calls. Cross-check it agrees with the HashMap-packed path.
    let ev = WasmEvaluator::new(W5_INLINE_SRC).expect("WasmEvaluator::new(W5 inline)");
    assert!(
        ev.has_fast_path(),
        "W5 inline must expose fast-path entry (i64 return is the Int result)"
    );
    let fast = ev
        .run_main_legacy_i64_fast(&[10_000])
        .expect("fast(W5, n=10_000)");
    assert_eq!(fast, expected_w5(10_000));
}

#[test]
fn w5_production_dict_source_still_scope_cuts() {
    // The production source binds `d: { a: 1, ..., j: 10 }` as a
    // `#internal` dict and a parallel `keys: ["a", ..., "j"]` list,
    // returning `Dict { d, keys, result }`. Until Z.4 lands real
    // IR-walker support for dict / list literals + bare-`Dict`
    // returns, this path must still surface a tree-walker fallback
    // tier — a silent fast-path pass on the production source would
    // be the paper-win anti-pattern called out in design §7.
    let prod_src = "#import list from \"std/list\"\n\
                    #main(Int n) -> Dict\n\
                    {\n\
                      #internal\n\
                      d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
                      #internal\n\
                      keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
                      result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
                    }";
    let ev = WasmEvaluator::new(prod_src).expect("WasmEvaluator::new(W5 production)");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W5 production Dict source must surface tree-walker fallback (Z.4 follow-up)"
    );
}
