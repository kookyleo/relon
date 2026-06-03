//! W5-P2: `Op::ConstListString` materialisation + `List<String>`
//! int-index, three-way aligned (tree-walk gold standard / cranelift
//! golden / LLVM AOT).
//!
//! ## What this pins
//!
//! A `["a", .., "j"][i]` index on a materialised `List<String>` is the
//! second W5-epic primitive (`keys[i % 10]`). Before P2 both cranelift
//! and LLVM rejected `Op::ConstListString` ("pointer-array
//! materialisation nowhere wired") and the `lower_variable` index path
//! handled only the inline-payload `ListInt` / `ListFloat` shapes — a
//! `List<String>` is a *pointer array* (`[len][off_i...]` whose `off_i`
//! is the arena-relative offset of a `[slen][utf8]` String record).
//!
//! 1. **IR shape** — the source lowers to `Op::ConstListString` plus
//!    the inline 4-byte-stride index addressing terminating in
//!    `LoadI32AtAbsolute` (the loaded `u32` IS a String handle) and a
//!    `EmitTailRecordFromAbsoluteAddr { ty: String }` return copy.
//! 2. **Codegen parity** — both backends accept the source through
//!    `from_source`; the cranelift golden's acceptance is the codegen
//!    oracle.
//! 3. **Value three-way** — tree-walk / cranelift / LLVM all return the
//!    same indexed String for every in-bounds index. The const-pool
//!    pointer-array layout is byte-identical across backends (pinned in
//!    each crate's `const_pool` unit tests).

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_ir::ir::{Op, TaggedOp};
use relon_parser::parse_document;

/// `List<String>` literal bound in a `where`, indexed by the `#main`
/// Int param, returned through a schema-wrapped `String` field. This is
/// the `from_source`-reachable shape for the W5 `keys[i]` primitive:
/// the branded `#main` body must be a dict literal, so the `where`-bound
/// list rides inside the field value's parenthesised expression.
const SRC: &str = "#schema R { String result: * }\n\
                   #main(Int i) -> R\n\
                   { result: (keys[i] where { keys: [\"a\",\"b\",\"c\",\"d\",\"e\"] }) }";

fn flatten_ops(src: &str) -> Vec<Op> {
    let options = relon_analyzer::AnalyzeOptions {
        strict_mode: false,
        ..Default::default()
    };
    let lowered = relon_ir::compile(src, &options).expect("frontend compile");
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

/// Pull the `result` String field out of the returned schema dict.
fn result_of(v: &Value) -> String {
    let dict = match v {
        Value::Dict(d) => d,
        other => panic!("expected schema Dict return, got {other:?}"),
    };
    match dict.map.get("result").expect("return dict has `result`") {
        Value::String(s) => s.to_string(),
        other => panic!("expected String `result`, got {other:?}"),
    }
}

/// Tree-walk gold standard for `SRC` at index `i`.
fn tree_walk(i: i64) -> String {
    let node = parse_document(SRC).expect("parse SRC");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("i".to_string(), Value::Int(i));
    result_of(&walker.run_main(&scope, args).expect("tree-walk run_main"))
}

#[test]
fn const_list_string_index_lowers_to_expected_ir() {
    let ops = flatten_ops(SRC);
    assert!(
        ops.iter().any(|o| matches!(o, Op::ConstListString { .. })),
        "SRC must lower to Op::ConstListString; ops:\n{ops:#?}"
    );
    // The index path loads the String handle (u32) out of the
    // pointer-array header and copies the `[slen][utf8]` record into
    // the return buffer's tail area — NOT via Op::ListGetByIntIdx
    // (that op stays trace-recorder-only; static codegen rejects it).
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::LoadI32AtAbsolute { .. })),
        "index must load the String handle via LoadI32AtAbsolute; ops:\n{ops:#?}"
    );
    assert!(
        ops.iter().any(|o| matches!(
            o,
            Op::EmitTailRecordFromAbsoluteAddr {
                ty: relon_ir::ir::IrType::String
            }
        )),
        "String return must copy via EmitTailRecordFromAbsoluteAddr{{String}}; ops:\n{ops:#?}"
    );
    assert!(
        !ops.iter().any(|o| matches!(o, Op::ListGetByIntIdx { .. })),
        "static codegen must NOT emit Op::ListGetByIntIdx; ops:\n{ops:#?}"
    );
}

#[test]
fn const_list_string_codegen_parity() {
    // Cranelift golden acceptance is the codegen oracle: it now carries
    // `list_string_offsets`, so the LLVM port must accept the same src.
    AotEvaluator::from_source(SRC)
        .unwrap_or_else(|e| panic!("cranelift golden must compile SRC:\n{SRC}\nerr: {e:?}"));
    LlvmAotEvaluator::from_source(SRC)
        .unwrap_or_else(|e| panic!("llvm backend must compile SRC:\n{SRC}\nerr: {e:?}"));
}

#[test]
fn const_list_string_index_three_way() {
    let llvm = LlvmAotEvaluator::from_source(SRC).expect("llvm compiles");
    let cl = AotEvaluator::from_source(SRC).expect("cranelift compiles");

    for (i, want) in [(0, "a"), (1, "b"), (2, "c"), (3, "d"), (4, "e")] {
        let oracle = tree_walk(i);
        assert_eq!(oracle, want, "tree-walk oracle sanity at i={i}");

        let mut a = HashMap::new();
        a.insert("i".to_string(), Value::Int(i));
        let got_llvm = result_of(&llvm.run_main(a.clone()).expect("llvm run_main"));
        let got_cl = result_of(&cl.run_main(a).expect("cranelift run_main"));

        assert_eq!(got_cl, oracle, "cranelift List<String> index diverged at i={i}");
        assert_eq!(got_llvm, oracle, "llvm List<String> index diverged at i={i}");
    }
}
