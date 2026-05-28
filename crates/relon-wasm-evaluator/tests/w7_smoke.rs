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
//! via `result: fib(n)`) is now lowered end-to-end through the IR
//! walker as of Phase Z.4.3 — see `z4_closure_smoke.rs` for the
//! positive coverage. The `w7_production_dict_source_runs_on_walker`
//! test below pins the W7-specific assertion: production source
//! must surface `Tier::Compiled` and `Value::Dict { result: Int(17711) }`
//! for `n = 22`.

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
fn w7_production_dict_source_runs_on_walker() {
    // Z.4.3 — production W7 source binds `fib: (k) => ...` as a
    // `#internal` first-class recursive closure inside a Dict-body
    // and returns `Dict { result: fib(n) }`. The IR pipeline lowers
    // this into the closure-as-value `MakeClosure` / `CallClosure`
    // primitives plus the Z.4.1 `AllocRootRecord` /
    // `StoreFieldAtRecord` Dict mini-ABI; Z.4.3 wires the funcref
    // table + `call_indirect` shape so the walker emits a real wasm
    // module instead of routing through the tree-walker fallback.
    //
    // Honesty (design §7):
    //
    // 1. Same algorithm? — yes, the recursive `fib` body lifts to a
    //    separate wasm function (`__closure_0`) called via
    //    `call_indirect` against the module's funcref table. No
    //    iterative `(a, b) <- (b, a + b)` rewrite, no Binet's
    //    closed-form trick. Each `fib(k)` call burns one wasm stack
    //    frame.
    // 2. Same code path? — yes, `WasmEvaluator::new` lowers via
    //    `relon-codegen-wasm`'s IR walker (not the W7 inline-Int
    //    classifier); the Dict-return + closure-as-value combo
    //    routes straight to `lower_ir_module`.
    // 3. Same I/O shape? — input `args["n"] = Int(22)`, output
    //    `Value::Dict { result: Int(17711) }` matching the
    //    tree-walker reference end-to-end.
    let prod_src = "#main(Int n) -> Dict\n\
                    {\n\
                      #internal\n\
                      fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                      result: fib(n)\n\
                    }";
    let ev = WasmEvaluator::new(prod_src).expect("WasmEvaluator::new(W7 production)");

    // Same I/O shape — pass `n = 22` (the bench point) and check
    // the matching Dict-return decode.
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(22));
    let out = ev.run_main(args).expect("run_main(W7 production, n=22)");

    let dict_map = match &out {
        Value::Dict(d) => &d.map,
        other => panic!("W7 production must return Value::Dict, got {other:?}"),
    };
    assert_eq!(
        dict_map.get("result").cloned(),
        Some(Value::Int(expected_w7(22))),
        "W7 production Dict.result must equal fib(22) (tree-walker reference)"
    );
    assert_eq!(
        dict_map.get("result").cloned(),
        Some(Value::Int(17711)),
        "W7 production Dict.result must equal 17711 (fib(22) per closed-form check)"
    );
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "W7 production Dict source must reach the compiled tier post Z.4.3 \
         (funcref table + call_indirect closure-as-value lowering)"
    );
}
