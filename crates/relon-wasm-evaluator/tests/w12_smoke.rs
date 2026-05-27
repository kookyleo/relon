//! W12 smoke: `x + 1` — the trivial happy-path. Source byte-identical
//! to `crates/relon-bench/benches/cmp_lua.rs::w12_relon_src()`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W12_SRC: &str = "#main(Int x) -> Int\nx + 1";

#[test]
fn w12_increment_int_matches_expected() {
    let ev = WasmEvaluator::new(W12_SRC).expect("WasmEvaluator::new(W12)");

    // Cold tier before any call.
    assert_eq!(ev.active_tier(), Tier::Cold);

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(41));
    let out = ev.run_main(args).expect("run_main(W12, x=41)");
    assert_eq!(out, Value::Int(42));

    // Compiled tier after a successful call.
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn w12_negative_input() {
    let ev = WasmEvaluator::new(W12_SRC).expect("WasmEvaluator::new(W12)");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(-1));
    let out = ev.run_main(args).expect("run_main(W12, x=-1)");
    assert_eq!(out, Value::Int(0));
}

#[test]
fn w12_fast_path_matches_buffer_path() {
    // Phase Z.3a contract: `run_main_legacy_i64_fast` must be
    // observably equivalent to the buffer-protocol `run_main` for any
    // Z.1 source the classifier accepts.
    let ev = WasmEvaluator::new(W12_SRC).expect("WasmEvaluator::new(W12)");
    assert!(ev.has_fast_path(), "W12 must expose the fast path");

    for x in [-7i64, 0, 1, 41, 100_000] {
        let fast = ev.run_main_legacy_i64_fast(&[x]).expect("fast path call");
        let mut args = HashMap::new();
        args.insert("x".to_string(), Value::Int(x));
        let slow = match ev.run_main(args).expect("buffer path call") {
            Value::Int(n) => n,
            other => panic!("W12 buffer path returned non-Int {other:?}"),
        };
        assert_eq!(fast, slow, "W12 fast/buffer mismatch at x={x}");
        assert_eq!(fast, x + 1, "W12 fast path returned wrong value at x={x}");
    }
}

#[test]
fn w12_fast_path_arity_mismatch_is_unsupported() {
    let ev = WasmEvaluator::new(W12_SRC).expect("WasmEvaluator::new(W12)");
    // Z.1 programs are arity-1; 0-arg or 2-arg should bounce as Unsupported.
    let err = ev
        .run_main_legacy_i64_fast(&[])
        .expect_err("0-arity must fail");
    let s = format!("{err}");
    assert!(
        s.contains("expects 1 arg") || s.contains("Z.1 programs"),
        "unexpected err shape: {s}"
    );
    let err2 = ev
        .run_main_legacy_i64_fast(&[1, 2])
        .expect_err("2-arity must fail");
    let s2 = format!("{err2}");
    assert!(
        s2.contains("expects 1 arg") || s2.contains("Z.1 programs"),
        "unexpected err shape: {s2}"
    );
}
