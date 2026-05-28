//! W8 smoke: the inline-Int dispatch variant of W8 (matching
//! `w8_relon_src_bytecode_dispatch()` in `crates/relon-bench/benches/cmp_lua.rs`)
//! lowered to WASM matches the polymorphic-dispatch sum
//! `Σ_{i in [0..n)} dispatch(i % 4)` over the 4-arm constant table.
//!
//! The 3 honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    `w8_relon_src_bytecode_dispatch()`. The lowered loop preserves
//!    the 4-arm dispatch via `br_table` (constant-time jump on the
//!    runtime tag value, not a constant-fold). **No** algebraic
//!    collapse to `(i % 4) + 1` — that closed-form is the
//!    `w8_relon_src_bytecode()` variant the bytecode row uses, but
//!    measuring it under the `relon_wasm_wasmtime` label would hide
//!    the polymorphic-dispatch cost W8 is meant to expose (paper-win
//!    anti-pattern per design §7).
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is `Value::Int(_)`.
//!
//! Note: the **production** W8 source (`#main(Int n) -> Dict` with an
//! `#internal dispatch: (tag) => ...` first-class closure called via
//! `dispatch(i % 4)`) still scope-cuts at the classifier and routes
//! through the tree-walker fallback — that path is Z.4 follow-up. See
//! `scope_cut_smoke.rs` for the scope-cut tier check pattern.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// Byte-identical to `w8_relon_src_bytecode_dispatch()` in cmp_lua.
const W8_INLINE_SRC: &str = "#import list from \"std/list\"\n\
                             #main(Int n) -> Int\n\
                             list.sum(range(n).map((i) =>\n\
                               (i % 4) == 0 ? 1 : (i % 4) == 1 ? 2 : (i % 4) == 2 ? 3 : 4))";

/// Tree-walker reference. Computes the same dispatch sum the lowered
/// `br_table` arms emit. A regression in the arm-to-value mapping
/// (e.g. swapping `tag == 2 → 3` for `tag == 2 → 4`) shows up as a
/// mismatch here rather than a silently-correct closed-form.
fn expected_w8(n: i64) -> i64 {
    let mut acc: i64 = 0;
    for i in 0..n {
        let tag = i % 4;
        let v: i64 = if tag == 0 {
            1
        } else if tag == 1 {
            2
        } else if tag == 2 {
            3
        } else {
            4
        };
        acc += v;
    }
    acc
}

#[test]
fn w8_handles_zero_n() {
    let ev = WasmEvaluator::new(W8_INLINE_SRC).expect("WasmEvaluator::new(W8 inline dispatch)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W8, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w8_handles_each_arm_individually() {
    // n=1..=4 visits each dispatch arm exactly once at the trailing
    // iteration. Catches a regression that swapped the arm constants
    // or used the wrong br_table label for a particular tag.
    for n in 1..=4 {
        let ev = WasmEvaluator::new(W8_INLINE_SRC).expect("WasmEvaluator::new(W8 inline dispatch)");
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev
            .run_main(args)
            .unwrap_or_else(|e| panic!("run_main(W8, n={n}): {e}"));
        assert_eq!(
            out,
            Value::Int(expected_w8(n)),
            "W8 dispatch arm mismatch at n={n} (each iter visits one new arm)"
        );
        assert_eq!(ev.active_tier(), Tier::Compiled);
    }
}

#[test]
fn w8_matches_tree_walker_small() {
    let ev = WasmEvaluator::new(W8_INLINE_SRC).expect("WasmEvaluator::new(W8 inline dispatch)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(16));
    let out = ev.run_main(args).expect("run_main(W8, n=16)");
    // n=16 = 4 full cycles → 4 * (1+2+3+4) = 40
    assert_eq!(out, Value::Int(expected_w8(16)));
    assert_eq!(out, Value::Int(40));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w8_matches_tree_walker_at_bench_n() {
    // Bench uses TREE_WALK_N = 10_000 (see cmp_lua.rs); drive the
    // same point so the smoke pins the bench's expected value end-
    // to-end. The constant is duplicated here intentionally — the
    // smoke crate doesn't depend on the bench fixtures.
    let ev = WasmEvaluator::new(W8_INLINE_SRC).expect("WasmEvaluator::new(W8 inline dispatch)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10_000));
    let out = ev.run_main(args).expect("run_main(W8, n=10000)");
    assert_eq!(out, Value::Int(expected_w8(10_000)));
    // n=10_000 = 2500 full cycles → 2500 * 10 = 25_000.
    assert_eq!(out, Value::Int(25_000));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w8_fast_path_round_trips() {
    // The fast path (`run_main_legacy_i64_fast`) shares the same
    // typed-func handle the bench's `relon_wasm_wasmtime_fast` row
    // calls. Cross-check it agrees with the HashMap-packed path.
    let ev = WasmEvaluator::new(W8_INLINE_SRC).expect("WasmEvaluator::new(W8 inline dispatch)");
    assert!(
        ev.has_fast_path(),
        "W8 inline dispatch must expose fast-path entry"
    );
    let fast = ev
        .run_main_legacy_i64_fast(&[10_000])
        .expect("fast(W8, n=10_000)");
    assert_eq!(fast, expected_w8(10_000));
}

#[test]
fn w8_production_dict_source_still_scope_cuts() {
    // The production source binds `dispatch: (tag) => ...` as a
    // `#internal` closure called via `dispatch(i % 4)` and returns
    // `Dict { dispatch, result }`. Phase Z.4.1 unlocked the bare-
    // `Dict` mini-ABI on the walker; W8 production stays scope-cut
    // upstream of the walker — the IR-pipeline's
    // `anon_dict_return_plan` rejects `list.sum(range(n).map(...))`
    // as the value for `result:` (the classifier only accepts
    // calls into previously-classified closure fields), so the
    // source never reaches the walker. Resolving this needs an IR-
    // side widening (track its own Z.4 follow-up); until then the
    // tree-walker fallback is the honest path — a silent fast-path
    // pass would be the paper-win anti-pattern from design §7.
    let prod_src = "#import list from \"std/list\"\n\
                    #main(Int n) -> Dict\n\
                    {\n\
                      #internal\n\
                      dispatch: (tag) => tag == 0 ? 1 : tag == 1 ? 2 : tag == 2 ? 3 : 4,\n\
                      result: list.sum(range(n).map((i) => dispatch(i % 4)))\n\
                    }";
    let ev = WasmEvaluator::new(prod_src).expect("WasmEvaluator::new(W8 production)");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W8 production Dict source must surface tree-walker fallback \
         (IR anon-Dict-plan rejects stdlib calls — separate follow-up)"
    );
}
