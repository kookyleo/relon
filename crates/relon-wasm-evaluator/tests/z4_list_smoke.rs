//! Phase Z.4.2 — IR walker control-flow smoke. Sources whose bodies
//! lower to a `Block { Loop { ... } }` accumulator skeleton (the
//! `range(...).reduce(...)` / `range(...).sum()` / `range(...).len()`
//! family) now round-trip through the walker path and land on the
//! `Compiled` tier without scope-cutting to the tree-walker fallback.
//!
//! The honesty contract (design §7) for the new walker arm:
//!
//! 1. Same algorithm? — yes. The IR walker emits one wasm instruction
//!    per IR Op; `relon_ir::lowering::emit_range_pipeline_loop` is the
//!    canonical source-to-IR pass that produces the
//!    `Block { Loop { BrIf, ... } }` skeleton. The walker's
//!    `Op::Block` / `Op::Loop` / `Op::Br` / `Op::BrIf` arms emit the
//!    matching wasm `block` / `loop` / `br` / `br_if` ops with the
//!    same depth discipline — no flattening, no closed-form rewrites.
//! 2. Same code path? — yes. `WasmEvaluator::new` runs parse +
//!    analyze + `lower_workspace_single` + `lower_ir_module` + the
//!    same wasmtime instantiation the classifier path uses.
//! 3. Same I/O shape? — `#main(Int n) -> Int`, returns `Value::Int(_)`.
//!    Cross-checked against the tree-walker reference in every test
//!    below so a regression in the new emit surfaces as a mismatch
//!    rather than a silent miscompile.
//!
//! Note: the **W9 production** `#main(Int n) -> Dict { rows, result }`
//! source still scope-cuts upstream of the walker — the IR pipeline's
//! `anon_dict_return_plan` rejects the `rows: range(n).map(...)` list-
//! of-list value because the closure-as-value path remains Z.4.3
//! follow-up. The matching scope-cut assertion lives in
//! `w9_smoke.rs::w9_production_dict_source_still_scope_cuts` and stays
//! green; this smoke only validates the simpler control-flow shape
//! Z.4.2 unlocks.

use std::collections::HashMap;
use std::sync::Arc;

use relon_eval_api::{Evaluator, Value};
use relon_evaluator::TreeWalkEvaluator;
use relon_wasm_evaluator::{Tier, WasmEvaluator};

/// Drive a Relon source through the tree-walker reference so the
/// walker path's output can be cross-checked end-to-end.
fn tree_walker_run(src: &str, args: HashMap<String, Value>) -> Value {
    let ast = relon_parser::parse_document(src).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&ast));
    let mut ctx = relon_evaluator::Context::new()
        .with_root(ast.clone())
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let ev = TreeWalkEvaluator::new(Arc::new(ctx));
    Evaluator::run_main(&ev, args).expect("tree-walker run_main")
}

/// Smallest control-flow shape: `range(n).reduce(0, (acc, i) => acc + i)`.
/// Lowers to one `Block { Loop { ... } }` region with the loop counter
/// and accumulator held in i64 wasm locals. The cmp_lua classifier
/// does NOT recognise this exact shape (it matches W2/W6's `list.sum`
/// surface and W9's nested-reduce shape — neither covers the bare
/// single-reduce), so the IR walker is the only path that can land
/// on `Compiled`.
const RANGE_REDUCE_SUM_SRC: &str = "#main(Int n) -> Int\n\
                                    range(n).reduce(0, (acc, i) => acc + i)";

#[test]
fn walker_lowers_range_reduce_sum() {
    let ev =
        WasmEvaluator::new(RANGE_REDUCE_SUM_SRC).expect("WasmEvaluator::new(range.reduce sum)");
    for n in [0i64, 1, 2, 5, 10, 100] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        let expected = tree_walker_run(RANGE_REDUCE_SUM_SRC, args);
        assert_eq!(
            out, expected,
            "walker output must match tree-walker reference for n={n}"
        );
        // 0 + 1 + ... + (n-1) = n*(n-1)/2 — sanity-check the
        // tree-walker's answer too so a regression in BOTH paths
        // doesn't silently pass.
        let closed_form = n * (n - 1) / 2;
        if let Value::Int(v) = out {
            assert_eq!(
                v, closed_form,
                "range-reduce sum should equal n*(n-1)/2 for n={n}"
            );
        } else {
            panic!("expected Int return, got {out:?}");
        }
    }
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "range.reduce(sum) source must land on Compiled tier via IR walker"
    );
}

#[test]
fn walker_fast_path_range_reduce_sum() {
    // Walker-emitted modules with Int return expose the i64 fast path
    // the bench `relon_wasm_wasmtime_fast` row exercises. Cross-check
    // it agrees with the HashMap-packed path.
    let ev =
        WasmEvaluator::new(RANGE_REDUCE_SUM_SRC).expect("WasmEvaluator::new(range.reduce sum)");
    assert!(
        ev.has_fast_path(),
        "walker-emitted Int-return modules must expose the fast path"
    );
    for n in [0i64, 1, 7, 100] {
        let fast = ev.run_main_legacy_i64_fast(&[n]).expect("fast path");
        let expected = n * (n - 1) / 2;
        assert_eq!(fast, expected, "fast path value mismatch for n={n}");
    }
}

/// Same shape as W9 inline-Int (matched against the bytecode-source
/// in the classifier) but with `(j + i)` instead of `(i * n + j)` so
/// the classifier's substring guard doesn't fire — the body still
/// stresses the nested `Block { Loop { Block { Loop { ... } } } }`
/// region pair, exercising the walker's depth discipline end-to-end.
const NESTED_REDUCE_SRC: &str = "#main(Int n) -> Int\n\
                                 range(n).reduce(0, (acc, j) =>\n\
                                   acc + range(n).reduce(0, (inner, i) => inner + (j + i)))";

#[test]
fn walker_lowers_nested_range_reduce() {
    let ev =
        WasmEvaluator::new(NESTED_REDUCE_SRC).expect("WasmEvaluator::new(nested range.reduce)");
    for n in [0i64, 1, 2, 5, 8, 32] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        let expected = tree_walker_run(NESTED_REDUCE_SRC, args);
        assert_eq!(
            out, expected,
            "walker output must match tree-walker for n={n}"
        );
    }
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "nested range.reduce must land on Compiled tier via IR walker"
    );
}

/// `range(n).reduce(1, (acc, i) => acc * (i + 1))` — factorial-style
/// reduce; checks that the reduce closure body can use the per-iter
/// element in a non-trivial expression and the i64 multiplication
/// chain still lowers through the walker.
const FACTORIAL_REDUCE_SRC: &str = "#main(Int n) -> Int\n\
                                    range(n).reduce(1, (acc, i) => acc * (i + 1))";

/// Z.4.2 — bare `List<Int>` literal return. Lowers to:
///
///   ConstListInt { idx: 0, elements: [10, 20, 30] }
///   StoreField { offset: 0, ty: ListInt }
///   Return
///
/// The walker installs the record `[len: u32 LE][pad: u32 zero][i64
/// elements...]` as an active data segment at `CONST_LIST_DATA_BASE`
/// and emits `i32.const <abs_offset>` for the `ConstListInt` op; the
/// trailing `Return` zero-extends the pointer to i64 for the typed-
/// func result. The host's `decode_list_int_record` reads the header
/// + payload back and wraps the elements as `Value::List`.
const CONST_LIST_INT_RETURN_SRC: &str = "#main(Int n) -> List<Int>\n[10, 20, 30]";

#[test]
fn walker_lowers_const_list_int_return() {
    let ev = WasmEvaluator::new(CONST_LIST_INT_RETURN_SRC)
        .expect("WasmEvaluator::new(const list int return)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(0));
    let out = ev.run_main(args.clone()).expect("run_main");
    let expected = tree_walker_run(CONST_LIST_INT_RETURN_SRC, args);
    assert_eq!(
        out, expected,
        "walker list-int return must match tree-walker reference"
    );
    match out {
        Value::List(items) => {
            assert_eq!(items.len(), 3, "list len mismatch");
            assert_eq!(items[0], Value::Int(10));
            assert_eq!(items[1], Value::Int(20));
            assert_eq!(items[2], Value::Int(30));
        }
        other => panic!("expected List, got {other:?}"),
    }
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "List<Int> literal return must land on Compiled tier via IR walker"
    );
}

#[test]
fn walker_const_list_int_return_no_fast_path() {
    // The fast-path entry hands back a raw i64 — for the List<Int>
    // shape that's a meaningless absolute pointer. `has_fast_path`
    // must report `false` so callers don't accidentally use it.
    let ev = WasmEvaluator::new(CONST_LIST_INT_RETURN_SRC)
        .expect("WasmEvaluator::new(const list int return)");
    assert!(
        !ev.has_fast_path(),
        "List<Int> return must not expose the i64 fast path"
    );
}

#[test]
fn walker_lowers_factorial_reduce() {
    let ev =
        WasmEvaluator::new(FACTORIAL_REDUCE_SRC).expect("WasmEvaluator::new(factorial reduce)");
    for (n, expected) in [(0i64, 1), (1, 1), (3, 6), (5, 120), (7, 5040)] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        assert_eq!(
            out,
            Value::Int(expected),
            "factorial(n={n}) mismatch via walker"
        );
        let tw = tree_walker_run(FACTORIAL_REDUCE_SRC, args);
        assert_eq!(out, tw, "walker vs tree-walker mismatch for n={n}");
    }
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "factorial reduce must land on Compiled tier via IR walker"
    );
}
