//! Phase Z.4.1 — IR-walker Dict-return smoke.
//!
//! Sources whose `#main(...) -> Dict { ... }` body lowers to a
//! `AllocRootRecord { idx } ... StoreFieldAtRecord ... Return` op
//! stream round-trip through the IR-walker tier and land on `Compiled`
//! with a `Value::Dict { ... }` return whose fields match the
//! tree-walker reference.
//!
//! Honesty contract (design §7):
//!
//! 1. Same algorithm? — the walker emits one wasm instruction per IR
//!    Op; each `StoreFieldAtRecord` becomes a typed `i64.store` at
//!    the record's per-field offset, no algorithmic substitution.
//! 2. Same code path? — `WasmEvaluator::new` runs parse + analyze +
//!    `lower_workspace_single` + `lower_ir_module`; the classifier
//!    path is bypassed because the production-Dict sources don't
//!    match any cmp_lua row.
//! 3. Same I/O shape? — `args["n"] = Int(n)`, return is
//!    `Value::Dict { field_0: Int(_), ... }` matching the tree-walker
//!    reference's bare-`Dict` shape (no brand — the IR's anon-Dict
//!    return reuses the canonical `Ret` schema name internally, but
//!    the user surface stays unbranded).

use std::collections::HashMap;
use std::sync::Arc;

use relon_eval_api::{Evaluator, Value};
use relon_evaluator::TreeWalkEvaluator;
use relon_wasm_evaluator::{Tier, WasmEvaluator};

/// Drive a Relon source through the tree-walker reference so the
/// walker path's `Value::Dict` output can be cross-checked.
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

#[test]
fn walker_dict_return_single_int_field_matches_tree_walker() {
    // Minimum-viable Dict-return shape: a single Int field whose
    // value is derived from the `#main` Int param. The walker
    // allocates a 1-field record via `__relon_arena_alloc(8, 8)`,
    // stores `n + 1` at offset 0, and returns the record-base ptr
    // zext'd to i64. The host decodes to `Value::Dict { result: Int(_) }`.
    let src = "#main(Int n) -> Dict\n{ result: n + 1 }";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(Dict single)");

    for n in [-5i64, 0, 1, 7, 42] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        let expected = tree_walker_run(src, args);
        assert_eq!(
            out, expected,
            "walker Dict-return must match tree-walker for n={n}"
        );
    }
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "Dict-return source must land on Compiled via IR walker"
    );
}

#[test]
fn walker_dict_return_multi_field_matches_tree_walker() {
    // Two Int fields. Stresses the per-field offset wiring: field 0
    // at offset 0, field 1 at offset 8 (Int's natural alignment).
    let src = "#main(Int n) -> Dict\n{ a: n, b: n + 1 }";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(Dict multi)");

    for n in [0i64, 1, 99] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        let expected = tree_walker_run(src, args);
        assert_eq!(
            out, expected,
            "walker multi-field Dict-return must match tree-walker for n={n}"
        );
    }
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn walker_dict_return_no_fast_path() {
    // The Dict-return path's typed-func i64 result is an arena
    // pointer, not a `Value::Int` scalar — the fast-path entry
    // would hand back the meaningless raw pointer under the
    // `wasmtime_fast` label, so it must NOT advertise as available.
    let src = "#main(Int n) -> Dict\n{ result: n }";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(Dict no-fast)");
    assert!(
        !ev.has_fast_path(),
        "Dict-return walker path must NOT advertise the fast-path entry (i64 is a record pointer, not a scalar Int)"
    );
}

#[test]
fn walker_dict_return_repeat_calls_stay_compiled() {
    // Repeated `run_main` calls should each allocate a fresh record
    // (the arena resets between calls via `HostState::reset`),
    // produce the right value, and keep the tier on `Compiled`.
    let src = "#main(Int n) -> Dict\n{ result: n * 2 }";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(Dict repeat)");

    for n in [1i64, 2, 3, 4, 5, 100, 1000] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args).expect("run_main");
        match out {
            Value::Dict(d) => {
                let result = d.map.get("result").expect("missing result field");
                assert_eq!(
                    *result,
                    Value::Int(n * 2),
                    "result field mismatch for n={n}"
                );
            }
            other => panic!("expected Value::Dict, got {other:?}"),
        }
    }
    assert_eq!(ev.active_tier(), Tier::Compiled);
}
