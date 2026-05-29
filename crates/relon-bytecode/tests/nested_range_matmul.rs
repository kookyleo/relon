//! AOT-2 — doubly-nested `range.map(range.map(...))` cell-reduction.
//!
//! Pins the bytecode-VM build + run path for the W19 matmul kernel's
//! per-matrix cell-sum shape after the nested range-chain peephole
//! landed in `relon-ir`. Before the peephole the inner bare
//! `range(...)` map surfaced as `UnknownStdlibMethod` (`range` is a
//! tree-walker host fn, not an IR stdlib entry) so the
//! `relon_bytecode` row was unreachable. The peephole emits a
//! doubly-nested i64 accumulator loop with NO intermediate list
//! materialised; each result is cross-checked against a hand-computed
//! reference.

use relon_bytecode::BytecodeEvaluator;
use relon_eval_api::value::Value;
use relon_eval_api::Evaluator;
use std::collections::HashMap;

fn run_int(ev: &BytecodeEvaluator, n: i64) -> i64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match ev.run_main(args).expect("run_main") {
        Value::Int(v) => v,
        other => panic!("unexpected result shape: {other:?}"),
    }
}

/// W19-shape inner `row.reduce` cell sum: Σ_i Σ_j (i * n + j) % 100.
#[test]
fn nested_reduce_cell_sum_runs() {
    let src = "#unstrict\n\
               #import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n).map((i) => range(n).map((j) => (i * n + j) % 100))\n\
                 .reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))";
    let ev = BytecodeEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("bytecode build failed: {e}"));
    for n in [0i64, 1, 2, 8, 16] {
        let mut expected = 0i64;
        for i in 0..n {
            for j in 0..n {
                expected = expected.wrapping_add((i.wrapping_mul(n).wrapping_add(j)) % 100);
            }
        }
        assert_eq!(run_int(&ev, n), expected, "mismatch at n={n}");
    }
}

/// Inner fold via `list.sum(row)`: Σ_i Σ_j (i + j) % 100.
#[test]
fn nested_list_sum_row_runs() {
    let src = "#unstrict\n\
               #import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n).map((i) => range(n).map((j) => (i + j) % 100))\n\
                 .reduce(0, (acc, row) => acc + list.sum(row))";
    let ev = BytecodeEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("bytecode build failed: {e}"));
    for n in [0i64, 1, 3, 16] {
        let mut expected = 0i64;
        for i in 0..n {
            for j in 0..n {
                expected = expected.wrapping_add((i.wrapping_add(j)) % 100);
            }
        }
        assert_eq!(run_int(&ev, n), expected, "mismatch at n={n}");
    }
}

/// The commuted combine `list.sum(row) + acc` folds the same way — the
/// peephole accepts the row-fold on either side of the `+`.
#[test]
fn nested_commuted_combine_runs() {
    let src = "#unstrict\n\
               #import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n).map((i) => range(n).map((j) => i * j))\n\
                 .reduce(0, (acc, row) => list.sum(row) + acc)";
    let ev = BytecodeEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("bytecode build failed: {e}"));
    let n = 12i64;
    let mut expected = 0i64;
    for i in 0..n {
        for j in 0..n {
            expected += i * j;
        }
    }
    assert_eq!(run_int(&ev, n), expected);
}
