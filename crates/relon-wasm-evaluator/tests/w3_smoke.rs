//! W3 smoke: `range(n).map((i) => "a").reduce("", (acc, s) => acc + s)`
//! lowered to WASM matches `"a" * n`.
//!
//! Uses the byte-identical production source from
//! `crates/relon-bench/benches/cmp_lua.rs::w3_relon_src()`. The 3
//! honesty questions (design §7):
//!
//! 1. Same algorithm? — yes, source string is duplicated verbatim from
//!    `w3_relon_src()`. The lowered loop performs `n` per-iter byte
//!    stores (`memory[ptr + i] = 'a'`), exactly the n per-step append
//!    work the source `reduce` chain does. No `"a".repeat(n)`
//!    closed-form substitution.
//! 2. Same code path? — yes, `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`, calls go through the `Evaluator` trait.
//!    The lowering calls `__relon_arena_alloc(n, 1)` once to reserve
//!    the destination buffer, then loops in pure WASM.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is
//!    `Value::String("a" * n)`. The fast-path entry
//!    (`run_main_legacy_i64_fast`) is intentionally disabled for W3
//!    (the i64 return encodes a `(ptr<<32 | len)` pair, not a scalar
//!    Int); `has_fast_path()` returns `false`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W3_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> String\n\
                       range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";

#[test]
fn w3_handles_zero_n() {
    let ev = WasmEvaluator::new(W3_SRC).expect("WasmEvaluator::new(W3 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args).expect("run_main(W3, n=0)");
    assert_eq!(out, Value::String("".into()));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w3_matches_tree_walker_small() {
    let ev = WasmEvaluator::new(W3_SRC).expect("WasmEvaluator::new(W3 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let out = ev.run_main(args).expect("run_main(W3, n=10)");
    assert_eq!(out, Value::String("aaaaaaaaaa".into()));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w3_matches_tree_walker_at_bench_n() {
    // Bench uses STRING_CONCAT_N = 2_000 (see cmp_lua.rs); drive the
    // same point so the smoke pins the bench's expected value end-to-
    // end. The constant is duplicated here intentionally — the smoke
    // crate doesn't depend on the bench fixtures.
    const STRING_CONCAT_N: usize = 2_000;
    let ev = WasmEvaluator::new(W3_SRC).expect("WasmEvaluator::new(W3 inline)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(STRING_CONCAT_N as i64));
    let out = ev.run_main(args).expect("run_main(W3, n=2000)");
    match out {
        Value::String(s) => {
            assert_eq!(s.len(), STRING_CONCAT_N);
            // Spot-check the content — the loop must have written 'a'
            // bytes, not whatever was previously in the arena.
            assert!(s.chars().all(|c| c == 'a'));
        }
        other => panic!("W3 expected String, got {other:?}"),
    }
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w3_fast_path_unavailable() {
    // W3 returns a packed (ptr<<32 | len) i64, not a scalar Int.
    // `has_fast_path()` must report false so the cmp_lua bench's
    // `relon_wasm_wasmtime_fast` row is not booked (it would record
    // a meaningless raw-i64 timing under that label otherwise).
    let ev = WasmEvaluator::new(W3_SRC).expect("WasmEvaluator::new(W3 inline)");
    assert!(
        !ev.has_fast_path(),
        "W3 must NOT expose fast-path entry (return is String, not Int)"
    );
    // Calling the fast entry directly should error out with an
    // Unsupported, not silently book a raw i64.
    match ev.run_main_legacy_i64_fast(&[10]) {
        Err(RuntimeError::Unsupported { reason }) => {
            assert!(
                reason.contains("non-Int") || reason.contains("ptr"),
                "W3 fast-path unsupported reason should mention non-Int return; got: {reason}"
            );
        }
        other => panic!("W3 fast-path expected Unsupported error, got {other:?}"),
    }
}

#[test]
fn w3_buffer_path_stable_across_calls() {
    // Run W3 twice with different `n` values. The arena resets between
    // calls, so the second call's `'a'` writes must overwrite the
    // first's (or the new ptr lands at the bumped-back arena origin,
    // either way — the returned string must reflect the second call's
    // `n` only).
    let ev = WasmEvaluator::new(W3_SRC).expect("WasmEvaluator::new(W3 inline)");

    let mut args5 = HashMap::new();
    args5.insert("n".to_string(), Value::Int(5));
    let out5 = ev.run_main(args5).expect("run_main(W3, n=5)");
    assert_eq!(out5, Value::String("aaaaa".into()));

    let mut args3 = HashMap::new();
    args3.insert("n".to_string(), Value::Int(3));
    let out3 = ev.run_main(args3).expect("run_main(W3, n=3)");
    assert_eq!(out3, Value::String("aaa".into()));
}
