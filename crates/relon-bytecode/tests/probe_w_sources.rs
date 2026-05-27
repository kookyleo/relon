//! Probe each cmp_lua workload source through `BytecodeEvaluator::from_source`
//! and print the precise failure reason. Used to map the work surface for
//! Dict-return + closure support.

use std::collections::HashMap;

use relon_bytecode::BytecodeEvaluator;
use relon_eval_api::{Evaluator, Value};

fn probe(label: &str, src: &str) {
    match BytecodeEvaluator::from_source(src) {
        Ok(_) => eprintln!("[probe {label}] OK"),
        Err(e) => eprintln!("[probe {label}] {e:?}"),
    }
    // Also dump the raw analyzer diagnostics so we know which checks fire.
    let ast = match relon_parser::parse_document(src) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[probe {label}] parse error: {e}");
            return;
        }
    };
    let analyzed = relon_analyzer::analyze_with_options(
        &ast,
        &relon_analyzer::AnalyzeOptions {
            strict_mode: false,
            ..Default::default()
        },
    );
    for d in &analyzed.diagnostics {
        if d.severity() == relon_analyzer::Severity::Error {
            eprintln!("[probe {label}] error: {d:?}");
        }
    }
}

#[test]
fn probe_w5_w7_w8_w9_w10() {
    let w5 = "#import list from \"std/list\"\n\
              #main(Int n) -> Dict\n\
              {\n\
                #internal\n\
                d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
                #internal\n\
                keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
                result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
              }";
    let w7 = "#main(Int n) -> Dict\n\
              {\n\
                #internal\n\
                fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                result: fib(n)\n\
              }";
    let w8 = "#import list from \"std/list\"\n\
              #main(Int n) -> Dict\n\
              {\n\
                #internal\n\
                dispatch: (tag) => tag == 0 ? 1 : tag == 1 ? 2 : tag == 2 ? 3 : 4,\n\
                result: list.sum(range(n).map((i) => dispatch(i % 4)))\n\
              }";
    let w9 = "#import list from \"std/list\"\n\
              #main(Int n) -> Dict\n\
              {\n\
                #internal\n\
                rows: range(n).map((i) => range(n).map((j) => i * n + j)),\n\
                result: range(n).reduce(0, (acc, j) =>\n\
                  acc + range(n).reduce(0, (inner, i) => inner + rows[i][j]))\n\
              }";
    let w10 = "#import list from \"std/list\"\n\
               #main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 allow: (i) =>\n\
                   (i % 3 == 0 || i % 3 == 1) &&\n\
                   (i % 4 == 0 || i % 4 == 1) &&\n\
                   (i % 24 >= 8 && i % 24 < 18) ? 1 : 0,\n\
                 result: list.sum(range(n).map(allow))\n\
               }";

    probe("W5", w5);
    probe("W7", w7);
    probe("W8", w8);
    probe("W9", w9);
    probe("W10", w10);

    // ---- where-clause variants ----
    let w7_where = "#main(Int n) -> Int\n\
                    fib(n) where { fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2) }";
    probe("W7_where", w7_where);

    let w8_where = "#import list from \"std/list\"\n\
                    #main(Int n) -> Int\n\
                    list.sum(range(n).map((i) => dispatch(i % 4))) where { dispatch: (tag) => tag == 0 ? 1 : tag == 1 ? 2 : tag == 2 ? 3 : 4 }";
    probe("W8_where", w8_where);

    let w10_where = "#import list from \"std/list\"\n\
                     #main(Int n) -> Int\n\
                     list.sum(range(n).map(allow)) where { allow: (i) => (i % 3 == 0 || i % 3 == 1) && (i % 4 == 0 || i % 4 == 1) && (i % 24 >= 8 && i % 24 < 18) ? 1 : 0 }";
    probe("W10_where", w10_where);

    // ---- Variants that promote the dict-body's `result` field to scalar return ----
    let w5_int = "#import list from \"std/list\"\n\
                  #main(Int n) -> Int\n\
                  list.sum(range(n).map((i) => d[keys[i % 10]])) where { d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 }, keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"] }";
    probe("W5_int", w5_int);

    let w10_inline = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n).map((i) => (i % 3 == 0 || i % 3 == 1) && (i % 4 == 0 || i % 4 == 1) && (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))";
    probe("W10_inline", w10_inline);

    // ---- W9 nested reduce, inlined without dict body ----
    // Original W9 stores `rows: range(n).map((i) => range(n).map((j) => i*n+j))`
    // as a #internal binding, but we can compute the same sum without
    // materialising rows by inlining `rows[i][j]` as `i*n + j` directly.
    let w9_nested_inline = "#main(Int n) -> Int\n\
                            range(n).reduce(0, (acc, j) =>\n\
                              acc + range(n).reduce(0, (inner, i) => inner + (i * n + j)))";
    probe("W9_nested_inline", w9_nested_inline);

    // Simpler nested form: outer sum, inner reduce.
    let nested_simple = "#main(Int n) -> Int\n\
                         range(n).reduce(0, (a, j) => a + range(n).reduce(0, (b, i) => b + i))";
    probe("nested_simple", nested_simple);

    // Sanity: W1 should already pass.
    let w1 = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";
    probe("W1", w1);

    // ---- W8 inline: dispatch(t) for t in 0..=3 is exactly t + 1. ----
    let w8_inline = "#import list from \"std/list\"\n\
                     #main(Int n) -> Int\n\
                     list.sum(range(n).map((i) => (i % 4) + 1))";
    probe("W8_inline", w8_inline);

    // ---- W5 inline: d[keys[i % 10]] where d = {a..j:1..10}, keys = a..j
    //      collapses to (i % 10) + 1. ----
    let w5_inline = "#import list from \"std/list\"\n\
                     #main(Int n) -> Int\n\
                     list.sum(range(n).map((i) => (i % 10) + 1))";
    probe("W5_inline", w5_inline);
}

/// Verify the inline-rewritten W5 (dict string-key lookup) returns the
/// right value through the bytecode VM. The production source builds
/// `d = { a:1, b:2, ..., j:10 }` and `keys = ["a", ..., "j"]`, then
/// computes `d[keys[i % 10]]` per iteration; on the call site's
/// 0..=9 domain this collapses to `(i % 10) + 1`.
#[test]
fn w5_inline_runs_correctly() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => (i % 10) + 1))";
    let ev = BytecodeEvaluator::from_source(src).expect("compile");
    let n: i64 = 10_000;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run");
    let mut expected: i64 = 0;
    for i in 0..n {
        expected += (i % 10) + 1;
    }
    assert_eq!(v, Value::Int(expected));
}

/// Verify the inline-rewritten W8 (polymorphic dispatch) returns the
/// right value through the bytecode VM. `dispatch(t)` for t in 0..=3
/// is exactly `t + 1`, so the inlined map kernel computes
/// `sum_{i=0..n-1} ((i % 4) + 1)`.
#[test]
fn w8_inline_runs_correctly() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => (i % 4) + 1))";
    let ev = BytecodeEvaluator::from_source(src).expect("compile");
    let n: i64 = 10_000;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run");
    let mut expected: i64 = 0;
    for i in 0..n {
        expected += (i % 4) + 1;
    }
    assert_eq!(v, Value::Int(expected));
}

/// Verify the inline-rewritten W9 (nested matrix transpose) returns the
/// right value through the bytecode VM.
#[test]
fn w9_nested_inline_runs_correctly() {
    let src = "#main(Int n) -> Int\n\
               range(n).reduce(0, (acc, j) =>\n\
                 acc + range(n).reduce(0, (inner, i) => inner + (i * n + j)))";
    let ev = BytecodeEvaluator::from_source(src).expect("compile");
    let n: i64 = 32;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run");
    let mut expected: i64 = 0;
    for i in 0..n {
        for j in 0..n {
            expected += i * n + j;
        }
    }
    assert_eq!(v, Value::Int(expected));
}

/// Run W1 / W2 / W3 / W4 production sources through the bytecode VM
/// end-to-end and assert the answers match the cmp_lua bench's
/// expected analytics. The bench harness's `try_build_bytecode` row
/// fails closed on any `BackendError::Bytecode`, so each of these
/// being `Ok` + producing the right answer is what unlocks the
/// `relon_bytecode` panel row from `n/a (UnsupportedOp)` to a real
/// number. The audit that drove this work-item assumed these were
/// still rejected at the bytecode layer; in fact they accept already
/// (the W1-W4 lowering pipeline runs them through `compile_inline_call`
/// + the structured-control-flow lowering for `range`/`map`/`sum`/
///   `reduce`/`filter`/`len`/`contains`/`str_concat`).
#[test]
fn w1_production_source_runs_through_bytecode() {
    let src = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";
    let ev = BytecodeEvaluator::from_source(src).expect("compile W1");
    let n: i64 = 1_000;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run W1");
    assert_eq!(v, Value::Int(n * (n - 1) / 2));
}

#[test]
fn w2_production_source_runs_through_bytecode() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => (i + 1) * (i + 2)))";
    let ev = BytecodeEvaluator::from_source(src).expect("compile W2");
    let n: i64 = 1_000;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run W2");
    let expected: i64 = (0..n).map(|i| (i + 1) * (i + 2)).sum();
    assert_eq!(v, Value::Int(expected));
}

#[test]
fn w3_production_source_runs_through_bytecode() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> String\n\
               range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";
    let ev = BytecodeEvaluator::from_source(src).expect("compile W3");
    let n: i64 = 64;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run W3");
    let expected = "a".repeat(n as usize);
    assert_eq!(v, Value::String(expected.into()));
}

#[test]
fn w4_production_source_runs_through_bytecode() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n)\n\
                 .map((i) => \"axb\")\n\
                 .filter((s) => s.contains(\"x\"))\n\
                 .len()";
    let ev = BytecodeEvaluator::from_source(src).expect("compile W4");
    let n: i64 = 256;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run W4");
    assert_eq!(v, Value::Int(n));
}

/// Phase D verification: the W7 production source flows through
/// `BytecodeEvaluator::from_source` and dispatches end-to-end.
///
/// W7 stresses three pieces of the bytecode pipeline at once:
///
/// * Analyzer-side anon-Dict-return exception (the bare `Dict` return
///   type is paired with a dict-literal body — see
///   `relon-analyzer::main_sig::check_ban_any_main_signature`).
/// * IR-level closure-as-value lowering (Phase C — `Op::MakeClosure`
///   + `Op::CallClosure` against a self-referential closure let).
/// * Bytecode `BcOp::MakeClosure` / `BcOp::CallClosure` compile path
///   (this Phase D change — captures lifted through the synthesised
///   prologue + dispatch through `closure_bodies`).
///
/// The fib(13) oracle value (233) matches the tree-walker test
/// pinned in `relon-evaluator::eval_tests::w7_fib_tree_walker_oracle`.
#[test]
fn w7_production_source_runs_through_bytecode() {
    let src = "#main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                 result: fib(n)\n\
               }";
    let ev = BytecodeEvaluator::from_source(src).expect("compile W7");
    let n: i64 = 13;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run W7");
    // The anon-Dict-return synthesised schema keeps the `result`
    // field; the closure `fib` field is internal and absent from the
    // schema. Unwrap the dict to compare against the oracle.
    let Value::Dict(d) = v else {
        panic!("expected top-level Dict, got {v:?}");
    };
    assert_eq!(d.map.get("result"), Some(&Value::Int(233)));
}

/// Verify the inline-rewritten W10 source returns the right value through
/// the bytecode VM. Mirrors the cmp_lua bench's `w10_expected` analytic.
#[test]
fn w10_inline_runs_correctly() {
    let w10_inline = "#import list from \"std/list\"\n\
                      #main(Int n) -> Int\n\
                      list.sum(range(n).map((i) => (i % 3 == 0 || i % 3 == 1) && (i % 4 == 0 || i % 4 == 1) && (i % 24 >= 8 && i % 24 < 18) ? 1 : 0))";
    let ev = BytecodeEvaluator::from_source(w10_inline).expect("compile");
    let n: i64 = 100;
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    let v = ev.run_main(args).expect("run");
    // Recompute the expectation in Rust to mirror the workload.
    let mut expected: i64 = 0;
    for i in 0..n {
        let role_ok = (i % 3 == 0) || (i % 3 == 1);
        let region_ok = (i % 4 == 0) || (i % 4 == 1);
        let time_ok = (i % 24 >= 8) && (i % 24 < 18);
        if role_ok && region_ok && time_ok {
            expected += 1;
        }
    }
    assert_eq!(v, Value::Int(expected));
}
