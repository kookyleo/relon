//! W10 smoke: the inline-Int variant of W10 (matching
//! `w10_relon_src_bytecode()` in `crates/relon-bench/benches/cmp_lua.rs`)
//! lowered to WASM matches the access-control count for `n` queries.
//!
//! The 3 honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    `w10_relon_src_bytecode()`. The lowered loop performs the three
//!    boolean predicates literally (role `% 3`, region `% 4`, hour `%
//!    24`) and accumulates `1` only when all three hold. No closed-
//!    form algebraic substitution.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is `Value::Int(_)`.
//!
//! Note: the **production** W10 source (`#main(Int n) -> Dict` with an
//! `#internal` closure `allow`) still scope-cuts at the classifier and
//! routes through the tree-walker fallback — that path is Z.4 follow-
//! up. See `scope_cut_smoke.rs` for the scope-cut tier check pattern.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// Byte-identical to `w10_relon_src_bytecode()` in cmp_lua.
const W10_INLINE_SRC: &str = "#import list from \"std/list\"\n\
                              #main(Int n) -> Int\n\
                              list.sum(range(n).map((i) =>\n\
                                (i % 3 == 0 || i % 3 == 1) &&\n\
                                (i % 4 == 0 || i % 4 == 1) &&\n\
                                (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))";

/// Tree-walker reference. Computes the access-control count by
/// evaluating each predicate independently per `i`. A regression in
/// the lowered loop (e.g. swapped modulus or wrong bound) shows up
/// as a mismatch here rather than a silently-correct closed-form.
fn expected_w10(n: i64) -> i64 {
    let mut count: i64 = 0;
    for i in 0..n {
        let role_i = i % 3;
        let region_i = i % 4;
        let hour = i % 24;
        let allow_role = role_i == 0 || role_i == 1;
        let allow_region = region_i == 0 || region_i == 1;
        let allow_hour = (8..18).contains(&hour);
        if allow_role && allow_region && allow_hour {
            count += 1;
        }
    }
    count
}

#[test]
fn w10_handles_zero_n() {
    let ev = WasmEvaluator::new(W10_INLINE_SRC).expect("WasmEvaluator::new(W10 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W10, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w10_matches_tree_walker_small() {
    let ev = WasmEvaluator::new(W10_INLINE_SRC).expect("WasmEvaluator::new(W10 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(50));
    let out = ev.run_main(args).expect("run_main(W10, n=50)");
    assert_eq!(out, Value::Int(expected_w10(50)));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w10_matches_tree_walker_at_bench_n() {
    // Bench uses CONFIG_QUERIES_N = 1_000 (see cmp_lua.rs); drive the
    // same point so the smoke pins the bench's expected value end-to-
    // end. The constant is duplicated here intentionally — the smoke
    // crate doesn't depend on the bench fixtures.
    let ev = WasmEvaluator::new(W10_INLINE_SRC).expect("WasmEvaluator::new(W10 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(1_000));
    let out = ev.run_main(args).expect("run_main(W10, n=1000)");
    assert_eq!(out, Value::Int(expected_w10(1_000)));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w10_fast_path_round_trips() {
    // The fast path (`run_main_legacy_i64_fast`) shares the same
    // typed-func handle the bench's `relon_wasm_wasmtime_fast` row
    // calls. Cross-check it agrees with the HashMap-packed path.
    let ev = WasmEvaluator::new(W10_INLINE_SRC).expect("WasmEvaluator::new(W10 inline)");
    assert!(ev.has_fast_path(), "W10 inline must expose fast-path entry");
    let fast = ev
        .run_main_legacy_i64_fast(&[1_000])
        .expect("fast(W10, n=1000)");
    assert_eq!(fast, expected_w10(1_000));
}

#[test]
fn w10_production_dict_source_still_scope_cuts() {
    // The production source binds `allow: (i) => ...` as a `#internal`
    // closure and returns `Dict { result: Int }`. Phase Z.4.1
    // unlocked the bare-`Dict` mini-ABI on the walker; Phase Z.4.3
    // unlocked the closure-as-value path (`MakeClosure` /
    // `CallClosure` + funcref-table dispatch). W10 production still
    // stays scope-cut **upstream** of the walker — the IR-pipeline's
    // `anon_dict_return_plan` rejects `list.sum(range(n).map(allow))`
    // as the value for `result:` (the classifier only accepts calls
    // into previously-classified closure fields), so the source never
    // reaches the walker even after Z.4.3. Resolving this needs an
    // IR-pipeline widening that recognises the `list.sum(... map(closure
    // ))` shape against the anon-Dict-return plan; until then the
    // tree-walker fallback is the honest path.
    let prod_src = "#import list from \"std/list\"\n\
                    #main(Int n) -> Dict\n\
                    {\n\
                      #internal\n\
                      allow: (i) =>\n\
                        (i % 3 == 0 || i % 3 == 1) &&\n\
                        (i % 4 == 0 || i % 4 == 1) &&\n\
                        (i % 24 >= 8 && i % 24 < 18) ? 1 : 0,\n\
                      result: list.sum(range(n).map(allow))\n\
                    }";
    let ev = WasmEvaluator::new(prod_src).expect("WasmEvaluator::new(W10 production)");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W10 production Dict source must surface tree-walker fallback (Z.4 follow-up)"
    );
}
