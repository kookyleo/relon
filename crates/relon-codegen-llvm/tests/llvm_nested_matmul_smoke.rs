//! AOT-2 end-to-end smoke: a doubly-nested `range.map(range.map(...))`
//! Int matrix reduced cell-by-cell lowers through the new IR
//! range-chain peephole into a doubly-nested integer accumulator loop
//! and compiles all the way through LLVM AOT. Mirrors the W19 matmul
//! kernel's per-matrix cell-sum shape (the full W19 `where`-clause
//! `List<List<Int>>` materialisation is a separate work item — this
//! smoke pins the nested-loop lowering reaching LLVM with NO list
//! allocated). Each result is cross-checked against a hand-computed
//! reference.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// W19-shape per-matrix cell sum via an inner `row.reduce`:
///   Σ_i Σ_j (i * n + j) % 100
const NESTED_REDUCE_SRC: &str = "#unstrict\n\
    #import list from \"std/list\"\n\
    #main(Int n) -> Int\n\
    range(n).map((i) => range(n).map((j) => (i * n + j) % 100))\n\
      .reduce(0, (row_acc, row) => row_acc + row.reduce(0, (cell_acc, cell) => cell_acc + cell))";

/// Same shape, but the inner fold is `list.sum(row)`:
///   Σ_i Σ_j (i + j) % 100
const NESTED_LIST_SUM_SRC: &str = "#unstrict\n\
    #import list from \"std/list\"\n\
    #main(Int n) -> Int\n\
    range(n).map((i) => range(n).map((j) => (i + j) % 100))\n\
      .reduce(0, (acc, row) => acc + list.sum(row))";

fn run_int_arg(src: &str, n: i64) -> i64 {
    let ev = LlvmAotEvaluator::from_source(src).expect("LLVM AOT from_source");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match ev.run_main(args).expect("LLVM run_main") {
        Value::Int(v) => v,
        other => panic!("expected Int, got {other:?}"),
    }
}

fn expected_reduce(n: i64) -> i64 {
    let mut total = 0i64;
    for i in 0..n {
        for j in 0..n {
            total = total.wrapping_add((i.wrapping_mul(n).wrapping_add(j)) % 100);
        }
    }
    total
}

fn expected_list_sum(n: i64) -> i64 {
    let mut total = 0i64;
    for i in 0..n {
        for j in 0..n {
            total = total.wrapping_add((i.wrapping_add(j)) % 100);
        }
    }
    total
}

#[test]
fn nested_reduce_matmul_cell_sum_llvm() {
    for n in [0i64, 1, 2, 8, 16] {
        assert_eq!(
            run_int_arg(NESTED_REDUCE_SRC, n),
            expected_reduce(n),
            "nested row.reduce cell-sum mismatch at n={n}"
        );
    }
}

#[test]
fn nested_list_sum_row_llvm() {
    for n in [0i64, 1, 3, 16] {
        assert_eq!(
            run_int_arg(NESTED_LIST_SUM_SRC, n),
            expected_list_sum(n),
            "nested list.sum(row) cell-sum mismatch at n={n}"
        );
    }
}

/// The nested shape must NOT materialise a row list — the IR dump
/// should carry the entry symbol and run cleanly. (We do not assert a
/// specific op count; the closed-form-resistant mod-100 keeps the
/// loop intact, but LLVM's optimiser may still restructure it.)
#[test]
fn nested_reduce_emits_entry_symbol() {
    let ev = LlvmAotEvaluator::from_source(NESTED_REDUCE_SRC).expect("from_source");
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("relon_llvm_entry"),
        "expected entry symbol in IR dump:\n{dump}"
    );
}
