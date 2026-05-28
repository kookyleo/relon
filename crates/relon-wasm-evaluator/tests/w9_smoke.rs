//! W9 smoke: the inline-Int variant of W9 (matching
//! `w9_relon_src_bytecode()` in `crates/relon-bench/benches/cmp_lua.rs`)
//! lowered to WASM matches the nested-reduce sum `Σ_j Σ_i (i*n + j)`.
//!
//! The 3 honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    `w9_relon_src_bytecode()`. The lowered nested loop preserves the
//!    O(n²) operation count — one `i64.mul` + two `i64.add`s per
//!    inner iteration. No closed-form `n²(n²-1)/2` substitution.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is `Value::Int(_)`.
//!
//! Note: the **production** W9 source (`#main(Int n) -> Dict` with an
//! `#internal rows: range(n).map(...)` list and a `rows[i][j]` lookup)
//! still scope-cuts at the classifier and routes through the tree-
//! walker fallback — that path is Z.4 follow-up. See
//! `scope_cut_smoke.rs` for the scope-cut tier check pattern.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// Byte-identical to `w9_relon_src_bytecode()` in cmp_lua.
const W9_INLINE_SRC: &str = "#main(Int n) -> Int\n\
                             range(n).reduce(0, (acc, j) =>\n\
                               acc + range(n).reduce(0, (inner, i) => inner + (i * n + j)))";

/// Tree-walker reference. Nested O(n²) sum, expressed as the same
/// double-loop the lowered WASM body emits — a regression in the
/// nested-loop emit (e.g. swapped outer/inner cursors, or stopping
/// the inner loop one iter short) shows up as a mismatch here rather
/// than a silently-correct closed-form.
fn expected_w9(n: i64) -> i64 {
    let mut outer: i64 = 0;
    for j in 0..n {
        let mut inner: i64 = 0;
        for i in 0..n {
            inner += i * n + j;
        }
        outer += inner;
    }
    outer
}

#[test]
fn w9_handles_zero_n() {
    let ev = WasmEvaluator::new(W9_INLINE_SRC).expect("WasmEvaluator::new(W9 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W9, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w9_handles_n_one() {
    // n=1: single iter `(i=0, j=0) -> 0*1+0 = 0`. Smallest non-degenerate
    // case; catches a swapped `n==0` short-circuit that would skip the
    // single iter.
    let ev = WasmEvaluator::new(W9_INLINE_SRC).expect("WasmEvaluator::new(W9 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(1));
    let out = ev.run_main(args).expect("run_main(W9, n=1)");
    assert_eq!(out, Value::Int(expected_w9(1)));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w9_matches_tree_walker_small() {
    let ev = WasmEvaluator::new(W9_INLINE_SRC).expect("WasmEvaluator::new(W9 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(8));
    let out = ev.run_main(args).expect("run_main(W9, n=8)");
    assert_eq!(out, Value::Int(expected_w9(8)));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w9_matches_tree_walker_at_bench_n() {
    // Bench uses W9_N = 32 (see cmp_lua.rs); drive the same point so
    // the smoke pins the bench's expected value end-to-end. The constant
    // is duplicated here intentionally — the smoke crate doesn't depend
    // on the bench fixtures.
    let ev = WasmEvaluator::new(W9_INLINE_SRC).expect("WasmEvaluator::new(W9 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(32));
    let out = ev.run_main(args).expect("run_main(W9, n=32)");
    assert_eq!(out, Value::Int(expected_w9(32)));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w9_fast_path_round_trips() {
    // The fast path (`run_main_legacy_i64_fast`) shares the same
    // typed-func handle the bench's `relon_wasm_wasmtime_fast` row
    // calls. Cross-check it agrees with the HashMap-packed path.
    let ev = WasmEvaluator::new(W9_INLINE_SRC).expect("WasmEvaluator::new(W9 inline)");
    assert!(ev.has_fast_path(), "W9 inline must expose fast-path entry");
    let fast = ev.run_main_legacy_i64_fast(&[32]).expect("fast(W9, n=32)");
    assert_eq!(fast, expected_w9(32));
}

#[test]
fn w9_production_dict_source_still_scope_cuts() {
    // The production source binds `rows: range(n).map(...)` as a
    // `#internal` list-of-list and returns `Dict { rows, result }`.
    // Phase Z.4.1 unlocked the bare-`Dict` mini-ABI on the walker;
    // W9 production stays scope-cut upstream of the walker — the
    // IR-pipeline's `anon_dict_return_plan` rejects the list-of-list
    // value for `rows:` (the classifier only accepts calls into
    // previously-classified closure fields), so the source never
    // reaches the walker. Resolving this needs both an IR-pipeline
    // widening AND the Z.4.2 List-literal walker arm; until then
    // the tree-walker fallback is the honest path.
    let prod_src = "#import list from \"std/list\"\n\
                    #main(Int n) -> Dict\n\
                    {\n\
                      #internal\n\
                      rows: range(n).map((i) => range(n).map((j) => i * n + j)),\n\
                      result: range(n).reduce(0, (acc, j) =>\n\
                        acc + range(n).reduce(0, (inner, i) => inner + rows[i][j]))\n\
                    }";
    let ev = WasmEvaluator::new(prod_src).expect("WasmEvaluator::new(W9 production)");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W9 production Dict source must surface tree-walker fallback (Z.4 follow-up)"
    );
}
