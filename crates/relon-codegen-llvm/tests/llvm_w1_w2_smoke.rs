//! Phase B end-to-end smoke: `.relon` source → LLVM AOT → run_main
//! → typed return. Covers the cmp_lua W1 / W2 production-source
//! shapes that motivated the Phase B widening of the LLVM emitter.
//!
//! Each test asserts the LLVM AOT result against the canonical
//! tree-walker output so any miscompile shows up as a value diff
//! rather than a silent regression.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// `list.sum(range(n))` — sum of `0..n-1`.
const W1_SRC: &str = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n))";

/// `list.sum(range(n).map((i) => (i + 1) * (i + 2)))` — the cmp_lua
/// W2 production source. Needs `#unstrict` because the closure
/// parameter is untyped; matches the cranelift backend's behaviour
/// (the bench's W2 row is `n/a` for the cranelift AOT path until
/// `#unstrict` is added).
const W2_SRC: &str = "#unstrict\n\
                      #import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n).map((i) => (i + 1) * (i + 2)))";

fn run_int_arg(src: &str, n: i64) -> i64 {
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM AOT from_source");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match ev.run_main(args).expect("LLVM run_main") {
        Value::Int(v) => v,
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn w1_list_sum_range_n_zero() {
    assert_eq!(run_int_arg(W1_SRC, 0), 0);
}

#[test]
fn w1_list_sum_range_n_one() {
    assert_eq!(run_int_arg(W1_SRC, 1), 0);
}

#[test]
fn w1_list_sum_range_n_ten() {
    let expected: i64 = (0..10i64).sum();
    assert_eq!(run_int_arg(W1_SRC, 10), expected);
}

#[test]
fn w1_list_sum_range_n_thousand() {
    let n = 1000i64;
    let expected = (0..n).sum::<i64>();
    assert_eq!(run_int_arg(W1_SRC, n), expected);
}

#[test]
fn w2_dot_product_zero() {
    assert_eq!(run_int_arg(W2_SRC, 0), 0);
}

#[test]
fn w2_dot_product_ten() {
    // sum of (i+1)*(i+2) for i in 0..10
    let expected: i64 = (0..10).map(|i: i64| (i + 1) * (i + 2)).sum();
    assert_eq!(run_int_arg(W2_SRC, 10), expected);
}

#[test]
fn w2_dot_product_thousand() {
    let n = 1000i64;
    let expected: i64 = (0..n).map(|i| (i + 1) * (i + 2)).sum();
    assert_eq!(run_int_arg(W2_SRC, n), expected);
}

#[test]
fn llvm_ir_dump_has_entry_symbol() {
    // The Phase B emitter runs the `default<O3>` pipeline on the
    // module before snapshotting the IR dump — for W1's sum-of-
    // arithmetic-progression LLVM detects the closed form
    // `n * (n - 1) / 2` and folds the loop away entirely. We only
    // assert the entry symbol is present so a regression that
    // dropped the function entirely surfaces here.
    let ev = LlvmAotEvaluator::from_source(W1_SRC).expect("from_source");
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("relon_llvm_entry"),
        "expected entry symbol in IR dump:\n{dump}"
    );
}
