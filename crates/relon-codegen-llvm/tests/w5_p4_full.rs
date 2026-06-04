//! W5-P4: the full production w5 Dict body compiles end-to-end and
//! matches the tree-walk gold standard on every native backend.
//!
//! `#main(Int n) -> Dict { #internal d:{a:1..j:10}, #internal
//! keys:["a".."j"], result: list.sum(range(n).map((i) => d[keys[i%10]])) }`
//!
//! P4 closes the dict-probe map loop: the anon-Dict classifier now
//! accepts a `#internal keys` `List<String>` field (lifted to an
//! `Op::ConstListString` captured `IrType::ListString` let-local, like
//! the P1 Dict field), and the inlined `list.sum(range.map(...))` loop
//! body resolves `keys[i % 10]` (P2 ListString int-index → String) then
//! `d[<String>]` (P3 IR-lowered dict-probe) through the captured `d` /
//! `keys` let-bindings — no `Op::DictGetByStringKey`, no new runtime
//! import. n=10 sums `d["a"]..d["j"]` = 1+2+…+10 = 55.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Op, TaggedOp};
use relon_parser::parse_document;

const W5_SRC: &str = "#import list from \"std/list\"\n#main(Int n) -> Dict\n{\n#internal\n\
     d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n#internal\n\
     keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
     result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n}";

/// The inline map body is not statically derivable by the strict
/// analyzer (the `d[keys[i%10]]` chain), so the dict-probe workload runs
/// through the non-strict envelope — the same `opts()` w2 / w5_inline use.
fn opts() -> relon_analyzer::AnalyzeOptions {
    relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    }
}

fn result_of(v: &Value) -> i64 {
    match v {
        Value::Dict(d) => match d.map.get("result").expect("return dict has `result`") {
            Value::Int(n) => *n,
            other => panic!("expected Int `result`, got {other:?}"),
        },
        other => panic!("expected dict return, got {other:?}"),
    }
}

fn tree_walk(n: i64) -> i64 {
    let node = parse_document(W5_SRC).expect("parse src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    result_of(&walker.run_main(&scope, args).expect("tree-walk run_main"))
}

fn flatten_ops() -> Vec<Op> {
    let lowered = relon_ir::compile(W5_SRC, &opts()).expect("frontend compile");
    let mut out = Vec::new();
    for func in &lowered.module.funcs {
        collect(&func.body, &mut out);
    }
    out
}

fn collect(body: &[TaggedOp], out: &mut Vec<Op>) {
    for t in body {
        out.push(t.op.clone());
        match &t.op {
            Op::Block { body, .. } | Op::Loop { body, .. } => collect(body, out),
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                collect(then_body, out);
                collect(else_body, out);
            }
            _ => {}
        }
    }
}

/// The `#internal d` / `#internal keys` fields materialise as const-pool
/// records and the probe is fully IR-lowered (no trace-recorder op).
#[test]
fn w5_full_lowers_to_const_records_and_probe() {
    let ops = flatten_ops();
    assert!(
        ops.iter().any(|o| matches!(o, Op::ConstDict { .. })),
        "w5 must materialise `d` via Op::ConstDict"
    );
    assert!(
        ops.iter().any(|o| matches!(o, Op::ConstListString { .. })),
        "w5 must materialise `keys` via Op::ConstListString"
    );
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::LoadI8UAtAbsolute { .. })),
        "dict probe must byte-compare via LoadI8UAtAbsolute"
    );
    assert!(
        !ops.iter().any(|o| matches!(o, Op::DictGetByStringKey { .. })),
        "static codegen must NOT emit Op::DictGetByStringKey"
    );
}

#[test]
fn w5_full_three_way() {
    let oracle = tree_walk(10);
    assert_eq!(oracle, 55, "tree-walk oracle: full w5 sum == 55");

    let cl = AotEvaluator::from_source_with_options(W5_SRC, &opts()).expect("cranelift compiles");
    let llvm = LlvmAotEvaluator::from_source_with_options(W5_SRC, &opts()).expect("llvm compiles");

    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let got_cl = result_of(&cl.run_main(args.clone()).expect("cranelift run_main"));
    let got_llvm = result_of(&llvm.run_main(args).expect("llvm run_main"));

    assert_eq!(got_cl, oracle, "cranelift full-w5 diverged from tree-walk");
    assert_eq!(got_llvm, oracle, "llvm full-w5 diverged from tree-walk");
}
