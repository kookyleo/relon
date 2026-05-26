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
                #private\n\
                d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
                #private\n\
                keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
                result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
              }";
    let w7 = "#main(Int n) -> Dict\n\
              {\n\
                #private\n\
                fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                result: fib(n)\n\
              }";
    let w8 = "#import list from \"std/list\"\n\
              #main(Int n) -> Dict\n\
              {\n\
                #private\n\
                dispatch: (tag) => tag == 0 ? 1 : tag == 1 ? 2 : tag == 2 ? 3 : 4,\n\
                result: list.sum(range(n).map((i) => dispatch(i % 4)))\n\
              }";
    let w9 = "#import list from \"std/list\"\n\
              #main(Int n) -> Dict\n\
              {\n\
                #private\n\
                rows: range(n).map((i) => range(n).map((j) => i * n + j)),\n\
                result: range(n).reduce(0, (acc, j) =>\n\
                  acc + range(n).reduce(0, (inner, i) => inner + rows[i][j]))\n\
              }";
    let w10 = "#import list from \"std/list\"\n\
               #main(Int n) -> Dict\n\
               {\n\
                 #private\n\
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
