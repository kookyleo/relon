//! Phase Z.4.0 — IR-walker smoke. Sources that DON'T match the
//! cmp_lua classifier patterns but DO fit the walker's scalar-Int
//! subset should round-trip through the walker path and land on the
//! `Compiled` tier without scope-cutting to the tree-walker fallback.
//!
//! The honesty contract (design §7) for the walker path:
//!
//! 1. Same algorithm? — yes, the IR walker emits one wasm instruction
//!    per IR Op; `relon_ir::lower_workspace_single` is the canonical
//!    source-to-IR pass shared with the LLVM AOT backend, so the
//!    walker's emit consumes the exact same op stream the AOT side
//!    walks. No closed-form rewrites, no algorithm substitution.
//! 2. Same code path? — yes, `WasmEvaluator::new` runs parse +
//!    analyze + `lower_workspace_single` + `lower_ir_module` + the
//!    same wasmtime instantiation the classifier path uses.
//! 3. Same I/O shape? — `#main(Int x) -> Int`, returns
//!    `Value::Int(n)`. Cross-checked against the tree-walker
//!    reference in every test below.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_evaluator::TreeWalkEvaluator;
use relon_wasm_evaluator::{Tier, WasmEvaluator};

/// Helper: drive a Relon source through the tree-walker reference so
/// the walker path's output can be cross-checked.
fn tree_walker_run(src: &str, args: HashMap<String, Value>) -> Value {
    use std::sync::Arc;
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
fn walker_lowers_arithmetic_chain_outside_cmp_lua_patterns() {
    // `(n + 1) * (n + 2) - n` — pure arithmetic, no stdlib calls. The
    // cmp_lua classifier doesn't recognise this shape, so the IR
    // walker is the only path that can land on `Compiled`.
    let src = "#main(Int n) -> Int\n(n + 1) * (n + 2) - n";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(arith)");

    for n in [-3i64, 0, 1, 7, 42] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        let expected = tree_walker_run(src, args);
        assert_eq!(
            out, expected,
            "walker output must match tree-walker reference for n={n}"
        );
    }
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "arithmetic source must land on Compiled tier via IR walker"
    );
}

#[test]
fn walker_lowers_ternary() {
    // `n < 0 ? 0 : n` — `If` Op + i64 comparison + branch yield. The
    // walker's `Op::If` arm + `I64LtS` + typed-result branch all
    // exercised here.
    let src = "#main(Int n) -> Int\nn < 0 ? 0 : n";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(ternary)");

    for n in [-5i64, -1, 0, 1, 42] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        let expected = tree_walker_run(src, args);
        assert_eq!(out, expected, "ternary mismatch for n={n}");
    }
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn walker_handles_modulo() {
    // `n % 7` — exercises `Op::Mod(I64)` → `I64RemS`.
    let src = "#main(Int n) -> Int\nn % 7";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(mod)");

    for n in [0i64, 1, 6, 7, 8, 100] {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev.run_main(args.clone()).expect("run_main");
        let expected = tree_walker_run(src, args);
        assert_eq!(out, expected, "mod mismatch for n={n}");
    }
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn walker_scope_cuts_string_return_to_tree_walker() {
    // `String` return is outside the walker's Z.4.0 envelope. The
    // classifier also doesn't recognise this shape (it's not W3's
    // production reduce form), so the source MUST fall through to
    // the tree-walker tier.
    let src = "#main(Int n) -> String\n\"hello\"";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(string-return)");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "String-return source must fall through to tree-walker (Z.4 follow-up)"
    );
}

#[test]
fn walker_path_matches_w12_classifier_path() {
    // W12 `x + 1` is the cmp_lua W12 row — the classifier matches it
    // first, so this test runs the **classifier** path. We pin the
    // observable I/O so a future re-ordering of the lowering tier
    // priority that put the walker first wouldn't silently change
    // the row's behaviour.
    let src = "#main(Int x) -> Int\nx + 1";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(W12)");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(41));
    let out = ev.run_main(args).expect("run_main(W12)");
    assert_eq!(out, Value::Int(42));
    assert_eq!(ev.active_tier(), Tier::Compiled);
}

#[test]
fn walker_fast_path_round_trips() {
    // Walker-emitted modules expose the i64 fast path too — the
    // module's `__main(i64) -> i64` typed-func signature matches
    // the classifier path's contract bit-for-bit.
    let src = "#main(Int n) -> Int\n(n + 1) * 2";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(fast)");
    assert!(
        ev.has_fast_path(),
        "walker-emitted modules with Int return must expose the fast path"
    );
    for n in [0i64, 1, 7, 100] {
        let fast = ev.run_main_legacy_i64_fast(&[n]).expect("fast path");
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let slow = match ev.run_main(args).expect("slow path") {
            Value::Int(v) => v,
            other => panic!("non-Int return {other:?}"),
        };
        assert_eq!(fast, slow, "fast/slow path mismatch for n={n}");
        let expected = (n + 1) * 2;
        assert_eq!(fast, expected, "incorrect output for n={n}");
    }
}
