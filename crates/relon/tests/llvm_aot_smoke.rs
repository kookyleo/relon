//! End-to-end smoke for the `Backend::LlvmAot` facade entry. Asserts
//! W1 / W2 production-source results match the tree-walker so any
//! regression in either the LLVM emitter or the facade wiring shows
//! up here as a value mismatch.
//!
//! Gated behind the `llvm-aot` feature (default-off) so the
//! workspace `cargo build` on hosts without LLVM 18 dev headers
//! stays green. Hosts that have the toolchain on the box run this
//! through `cargo test -p relon --features llvm-aot --test
//! llvm_aot_smoke`.

#![cfg(feature = "llvm-aot")]

use std::collections::HashMap;

use relon::{new_evaluator, Backend};
use relon_eval_api::Value;

const W1_SRC: &str = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n))";

const W2_SRC: &str = "#unstrict\n\
                      #import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n).map((i) => (i + 1) * (i + 2)))";

fn run_w(src: &str, n: i64) -> Value {
    let ev = new_evaluator(src, Backend::LlvmAot).expect("LLVM AOT setup");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    ev.run_main(args).expect("LLVM AOT run_main")
}

#[test]
fn facade_llvm_aot_w1_matches_native() {
    let n = 1000i64;
    let expected: i64 = (0..n).sum();
    assert_eq!(run_w(W1_SRC, n), Value::Int(expected));
}

#[test]
fn facade_llvm_aot_w2_matches_native() {
    let n = 1000i64;
    let expected: i64 = (0..n).map(|i: i64| (i + 1) * (i + 2)).sum();
    assert_eq!(run_w(W2_SRC, n), Value::Int(expected));
}

#[test]
fn facade_llvm_aot_w1_matches_tree_walker() {
    let src = W1_SRC;
    let n = 13i64;
    let llvm = run_w(src, n);
    let tw = {
        let ev = new_evaluator(src, Backend::TreeWalk).expect("TreeWalk setup");
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        ev.run_main(args).expect("TreeWalk run_main")
    };
    assert_eq!(llvm, tw, "LLVM result {llvm:?} != tree-walker {tw:?}");
}

#[test]
fn facade_llvm_aot_w2_matches_tree_walker() {
    let src = W2_SRC;
    let n = 17i64;
    let llvm = run_w(src, n);
    let tw = {
        let ev = new_evaluator(src, Backend::TreeWalk).expect("TreeWalk setup");
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        ev.run_main(args).expect("TreeWalk run_main")
    };
    assert_eq!(llvm, tw, "LLVM result {llvm:?} != tree-walker {tw:?}");
}
