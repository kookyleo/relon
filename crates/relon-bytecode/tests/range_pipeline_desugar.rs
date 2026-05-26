//! Open follow-up #2 — IR lowering surface expansion.
//!
//! These tests pin the bytecode-VM build path for the cmp_lua
//! W2 / W4 / W6 workloads after the range-pipeline peephole landed in
//! `relon-ir`. Before the peephole all three lowered through
//! `list_int_map` / `list_int_filter` / `list_int_length` whose
//! buffer-protocol bodies the bytecode scalar envelope rejects, so
//! the cmp_lua `relon_bytecode` row showed `n/a` for each.
//!
//! Coverage:
//!   * `list.sum(range(n).map(closure))` → W2 / W6 shape (Int return).
//!   * `range(n).map(c1).filter(c2).len()` → W4 shape (count over a
//!     filtered transient list).
//!   * Strict-mode-only diagnostics no longer gate the bytecode build
//!     (`ClosureParamTypeMissing` was the typical blocker — the
//!     tree-walker has always tolerated it).

use relon_bytecode::BytecodeEvaluator;
use relon_eval_api::value::Value;
use relon_eval_api::Evaluator;
use std::collections::HashMap;

fn build(src: &str) -> BytecodeEvaluator {
    BytecodeEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("bytecode build failed for source:\n{src}\nerror: {e}"))
}

fn run_int(ev: &BytecodeEvaluator, n: i64) -> i64 {
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match ev.run_main(args).expect("run_main") {
        Value::Int(v) => v,
        other => panic!("unexpected result shape: {other:?}"),
    }
}

/// W2 cmp_lua shape — `(i+1)*(i+2)` folded across `range(n)` via
/// `list.sum(range(n).map(...))`. Without the peephole the
/// `list_int_map` body would force the scalar envelope to bail.
#[test]
fn w2_shape_sum_map_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => (i + 1) * (i + 2)))";
    let ev = build(src);
    let n: i64 = 100;
    let expected: i64 = (0..n).map(|i| (i + 1) * (i + 2)).sum();
    assert_eq!(run_int(&ev, n), expected);
}

/// W6 cmp_lua shape — `list.sum(range(n).map((i) => i + 1))`.
#[test]
fn w6_shape_sum_map_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => i + 1))";
    let ev = build(src);
    let n: i64 = 100;
    let expected: i64 = (0..n).map(|i| i + 1).sum();
    assert_eq!(run_int(&ev, n), expected);
}

/// Chained map (`.map(...).map(...)`) collapses into the same single
/// loop. Sanity that the chain recogniser keeps peeling stages.
#[test]
fn chained_map_sum_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => i + 1).map((j) => j * 2))";
    let ev = build(src);
    let n: i64 = 50;
    let expected: i64 = (0..n).map(|i| (i + 1) * 2).sum();
    assert_eq!(run_int(&ev, n), expected);
}

/// W4 cmp_lua shape — `range(n).map((i) => "axb").filter((s) =>
/// s.contains("x")).len()`. The `.len()` consumer is a separate
/// peephole sharing the same `emit_range_pipeline_loop` core.
#[test]
fn w4_shape_map_filter_len_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n)\n\
                 .map((i) => \"axb\")\n\
                 .filter((s) => s.contains(\"x\"))\n\
                 .len()";
    let ev = build(src);
    let n: i64 = 100;
    // Every element contains "x", so the count equals n.
    assert_eq!(run_int(&ev, n), n);
}

/// `range(n).filter(p).len()` (no map) and `range(n).len()` (no stages)
/// both go through the same peephole — covers the 0-stage
/// terminator path.
#[test]
fn bare_range_len_runs() {
    let src = "#main(Int n) -> Int\nrange(n).len()";
    let ev = build(src);
    assert_eq!(run_int(&ev, 7), 7);
    assert_eq!(run_int(&ev, 0), 0);
}

#[test]
fn range_filter_len_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n).filter((i) => i % 2 == 0).len()";
    let ev = build(src);
    let n: i64 = 11;
    // Even count in 0..11 = 6 (0,2,4,6,8,10).
    assert_eq!(run_int(&ev, n), 6);
}

/// Sources the tree-walker accepts but that strict-mode flagged with
/// `ClosureParamTypeMissing` previously crashed the bytecode build at
/// the `analyzed.has_errors()` gate. Open follow-up #2 relaxes the
/// gate by running the analyzer with `strict_mode: false`. This test
/// pins the relaxation so a future tightening of strict mode doesn't
/// silently re-break W2 / W6.
#[test]
fn strict_only_diagnostic_no_longer_blocks_build() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               list.sum(range(n).map((i) => i + 1))";
    // No `#unstrict` directive — relies on the bytecode-side opt-out.
    let _ev = BytecodeEvaluator::from_source(src)
        .expect("strict-only ClosureParamTypeMissing should not block bytecode build");
}

/// `range(n).reduce(init, (acc, elem) => body)` covers integer
/// folds. Mirrors W3's accumulator update without the string
/// arena dependency so the test stays self-contained.
#[test]
fn int_reduce_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n).reduce(0, (acc, i) => acc + i)";
    let ev = build(src);
    let n: i64 = 10;
    let expected: i64 = (0..n).sum();
    assert_eq!(run_int(&ev, n), expected);
}

/// W3 cmp_lua shape — `range(n).map((i) => "a").reduce("", (acc, s)
/// => acc + s)`. Exercises the string-accumulator path that depends
/// on the B-1 / B-2 string-arena infrastructure.
#[test]
fn w3_shape_string_reduce_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> String\n\
               range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";
    let ev = BytecodeEvaluator::from_source(src)
        .unwrap_or_else(|e| panic!("W3 bytecode build failed: {e}"));
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(5));
    let v = ev.run_main(args).expect("run_main");
    match v {
        Value::String(s) => assert_eq!(s, "aaaaa"),
        other => panic!("W3 unexpected result: {other:?}"),
    }
}

/// Mixed `map().filter().reduce()` pipeline. Stages compose without
/// the emitter dropping the filter short-circuit or losing the
/// reduce accumulator type across iterations.
#[test]
fn mixed_map_filter_reduce_runs() {
    let src = "#import list from \"std/list\"\n\
               #main(Int n) -> Int\n\
               range(n)\n\
                 .map((i) => i * 2)\n\
                 .filter((j) => j > 4)\n\
                 .reduce(0, (acc, k) => acc + k)";
    let ev = build(src);
    let n: i64 = 10;
    let expected: i64 = (0..n).map(|i| i * 2).filter(|j| *j > 4).sum();
    assert_eq!(run_int(&ev, n), expected);
}
