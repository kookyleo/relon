//! W7 smoke: the inline-Int variant of W7 (matching
//! `w7_relon_src_bytecode()` in `crates/relon-bench/benches/cmp_lua.rs`)
//! lowered to WASM matches the doubly-recursive fib `fib(n) =
//! fib(n - 1) + fib(n - 2)` with `fib(0) = 0`, `fib(1) = 1`.
//!
//! The 3 honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    the `where`-clause sibling. The lowered module emits two local
//!    wasm functions (`$fib` + `$__main`); `$fib`'s body is the
//!    literal `if k < 2 { k } else { fib(k - 1) + fib(k - 2) }` with
//!    two direct `Call(fib_fn_idx)` instructions on the non-base arm.
//!    **No iterative `(a, b) <- (b, a + b)` rewrite** (the canonical
//!    W7 algorithm-substitution trap, user-flagged red line). **No
//!    closed-form Binet's formula** for the same reason.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//!    Recursive calls stay inside the wasm module (no host boundary
//!    crossing per recursive step).
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is `Value::Int(_)`.
//!
//! Note: the **production** W7 source (`#main(Int n) -> Dict` with an
//! `#internal fib: (k) => ...` first-class recursive closure called
//! via `result: fib(n)`) still scope-cuts at the classifier and
//! routes through the tree-walker fallback — that path is Z.4
//! follow-up. See `scope_cut_smoke.rs` for the scope-cut tier check
//! pattern, and the `w7_production_dict_source_still_scope_cuts`
//! test below for the W7-specific assertion.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// Byte-identical to `w7_relon_src_bytecode()` in cmp_lua (`where`-form
// sibling of the dict-bodied production source).
const W7_INLINE_SRC: &str = "#main(Int n) -> Int\n\
                             fib(n) where { fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2) }";

/// Tree-walker reference. Doubly-recursive fib matching the lowered
/// `$fib` body exactly — a regression in the base-case predicate or
/// recursive-arm wiring shows up as a mismatch here rather than a
/// silently-correct closed-form / iterative substitution.
fn expected_w7(k: i64) -> i64 {
    if k < 2 {
        k
    } else {
        expected_w7(k - 1) + expected_w7(k - 2)
    }
}

#[test]
fn w7_handles_zero() {
    let ev = WasmEvaluator::new(W7_INLINE_SRC).expect("WasmEvaluator::new(W7 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W7, n=0)");
    assert_eq!(out, Value::Int(0));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w7_handles_one() {
    // fib(1) = 1 — base-case branch hits the `k < 2` arm for k = 1.
    let ev = WasmEvaluator::new(W7_INLINE_SRC).expect("WasmEvaluator::new(W7 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(1));
    let out = ev.run_main(args).expect("run_main(W7, n=1)");
    assert_eq!(out, Value::Int(1));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w7_handles_two() {
    // fib(2) = 1 — smallest non-degenerate case. Materialises one
    // recursive descent down to `fib(0) + fib(1)` then back up.
    let ev = WasmEvaluator::new(W7_INLINE_SRC).expect("WasmEvaluator::new(W7 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(2));
    let out = ev.run_main(args).expect("run_main(W7, n=2)");
    assert_eq!(out, Value::Int(1));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w7_matches_tree_walker_small() {
    // fib(10) = 55 — exercises ~177 recursive calls, deep enough to
    // expose any stack-frame leak / wrong-arm regression.
    let ev = WasmEvaluator::new(W7_INLINE_SRC).expect("WasmEvaluator::new(W7 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let out = ev.run_main(args).expect("run_main(W7, n=10)");
    assert_eq!(out, Value::Int(expected_w7(10)));
    assert_eq!(out, Value::Int(55));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w7_matches_tree_walker_at_bench_n() {
    // Bench uses FIB_N = 22 (see cmp_lua.rs); drive the same point so
    // the smoke pins the bench's expected value end-to-end. The
    // constant is duplicated here intentionally — the smoke crate
    // doesn't depend on the bench fixtures. fib(22) = 17711.
    let ev = WasmEvaluator::new(W7_INLINE_SRC).expect("WasmEvaluator::new(W7 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(22));
    let out = ev.run_main(args).expect("run_main(W7, n=22)");
    assert_eq!(out, Value::Int(expected_w7(22)));
    assert_eq!(out, Value::Int(17711));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w7_fast_path_round_trips() {
    // The fast path (`run_main_legacy_i64_fast`) shares the same
    // typed-func handle the bench's `relon_wasm_wasmtime_fast` row
    // calls. Cross-check it agrees with the HashMap-packed path.
    let ev = WasmEvaluator::new(W7_INLINE_SRC).expect("WasmEvaluator::new(W7 inline)");
    assert!(ev.has_fast_path(), "W7 inline must expose fast-path entry");
    let fast = ev.run_main_legacy_i64_fast(&[22]).expect("fast(W7, n=22)");
    assert_eq!(fast, expected_w7(22));
    assert_eq!(fast, 17711);
}

#[test]
fn w7_production_dict_source_still_scope_cuts() {
    // The production source binds `fib: (k) => ...` as a `#internal`
    // first-class recursive closure inside a Dict-body and returns
    // `Dict { result: fib(n) }`. Phase Z.4.1 unlocked the
    // bare-`Dict` return path (`AllocRootRecord` /
    // `StoreFieldAtRecord` /  Dict mini-ABI lowering — see
    // `z4_dict_return_smoke.rs` for the positive coverage), but the
    // closure-as-value primitives (`MakeClosure` / `CallClosure`)
    // the IR pipeline emits for the `#internal fib` binding stay
    // scope-cut at the walker — that's Z.4.3 follow-up. Until Z.4.3
    // lands the funcref-table + `call_indirect` shape this source
    // must still surface a tree-walker fallback tier — a silent
    // fast-path pass on the production source would be the paper-
    // win anti-pattern called out in design §7.
    let prod_src = "#main(Int n) -> Dict\n\
                    {\n\
                      #internal\n\
                      fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                      result: fib(n)\n\
                    }";
    let ev = WasmEvaluator::new(prod_src).expect("WasmEvaluator::new(W7 production)");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W7 production Dict source must surface tree-walker fallback (Z.4.3 closure follow-up)"
    );
}
