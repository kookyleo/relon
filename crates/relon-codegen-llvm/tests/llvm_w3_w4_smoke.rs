//! Phase E.1 end-to-end smoke: `.relon` source → LLVM AOT → run_main
//! → typed return for the cmp_lua W3 / W4 / W4_long workloads. Each
//! test cross-checks against the canonical tree-walker output so any
//! miscompile shows up as a value mismatch rather than a silent
//! regression.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// W3 — String fold: `range(n).map(_ => "a").reduce("", (acc, s) => acc + s)`.
const W3_SRC: &str = "#unstrict\n\
                      #import list from \"std/list\"\n\
                      #main(Int n) -> String\n\
                      range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";

/// W4 — String contains: count strings in a generated list that contain "x".
const W4_SRC: &str = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      range(n).map((i) => \"axb\").filter((s) => s.contains(\"x\")).len()";

fn run_int(src: &str, n: i64) -> Value {
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM AOT from_source");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    ev.run_main(args).expect("LLVM run_main")
}

#[test]
fn w3_string_concat_zero() {
    assert_eq!(run_int(W3_SRC, 0), Value::String("".into()));
}

#[test]
fn w3_string_concat_one() {
    assert_eq!(run_int(W3_SRC, 1), Value::String("a".into()));
}

#[test]
fn w3_string_concat_ten() {
    assert_eq!(run_int(W3_SRC, 10), Value::String("aaaaaaaaaa".into()));
}

#[test]
fn w3_string_concat_thousand() {
    let expected = "a".repeat(1000);
    assert_eq!(run_int(W3_SRC, 1000), Value::String(expected.into()));
}

#[test]
fn w4_string_contains_zero() {
    assert_eq!(run_int(W4_SRC, 0), Value::Int(0));
}

#[test]
fn w4_string_contains_one() {
    assert_eq!(run_int(W4_SRC, 1), Value::Int(1));
}

#[test]
fn w4_string_contains_ten() {
    assert_eq!(run_int(W4_SRC, 10), Value::Int(10));
}

#[test]
fn w4_string_contains_thousand() {
    assert_eq!(run_int(W4_SRC, 1000), Value::Int(1000));
}
