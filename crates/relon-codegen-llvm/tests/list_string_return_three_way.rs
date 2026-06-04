//! `#main() -> List<String>` top-level return — cross-backend parity.
//!
//! A pure `List<String>` return is marshalled by the cranelift /
//! llvm-native `StoreField { ty: ListString }` path: the whole
//! pointer-array record (header `[len][off_0..]` plus the per-entry
//! `[slen][utf8]` String records) is copied into the output buffer's
//! tail and every inner offset relocated into the buffer's coordinate
//! system (`emit_store_field_list_string` / `emit_store_list_string`).
//!
//! This coverage is independent of any effectful builtin: the source
//! record is a const-pool `ConstListString` blob (a list literal). It
//! pins the `List<String>` return marshalling on all three native
//! executors (tree-walk gold standard + cranelift + llvm).

use std::collections::HashMap;

use relon_codegen_cranelift::AotEvaluator;
use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};

/// Pull the entry strings out of the returned `List<String>`.
fn result_of(v: &Value) -> Vec<String> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|e| match e {
                Value::String(s) => s.to_string(),
                other => panic!("expected String element, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List<String> return, got {other:?}"),
    }
}

fn arg_i(i: i64) -> HashMap<String, Value> {
    HashMap::from([("i".to_string(), Value::Int(i))])
}

/// Tree-walk gold-standard listing for a source string.
fn run_tree_walk(src: &str, i: i64) -> Vec<String> {
    use relon_evaluator::{Context, TreeWalkEvaluator};
    use relon_parser::parse_document;
    let node = parse_document(src).expect("parse");
    let analyzed = std::sync::Arc::new(relon_analyzer::analyze(&node));
    let ctx = Context::new()
        .with_root(node)
        .with_analyzed(std::sync::Arc::clone(&analyzed));
    let ctx = std::sync::Arc::new({
        let mut ctx = ctx;
        TreeWalkEvaluator::prepare_in_place(&mut ctx);
        ctx
    });
    let v = TreeWalkEvaluator::new(std::sync::Arc::clone(&ctx))
        .run_main(
            &std::sync::Arc::new(relon_eval_api::scope::Scope::default()),
            arg_i(i),
        )
        .expect("tree-walk run_main");
    result_of(&v)
}

/// A const `#main(Int i) -> List<String>` literal returns the same
/// bytes on all three native backends — the `StoreField { ty:
/// ListString }` pointer-array marshalling (header + inner-offset
/// relocation) exercised straight from the const-pool `ConstListString`
/// blob.
#[test]
fn const_list_string_return_three_way() {
    const SRC: &str = "#main(Int i) -> List<String>\n[\"b\", \"a\", \"c\"]";
    let want = vec!["b".to_string(), "a".to_string(), "c".to_string()];

    let tw = run_tree_walk(SRC, 0);
    assert_eq!(tw, want, "tree-walk const List<String> return mismatch");

    let cl = AotEvaluator::from_source(SRC).expect("cranelift from_source");
    let llvm = LlvmAotEvaluator::from_source(SRC).expect("llvm from_source");

    let cl_v = result_of(&cl.run_main(arg_i(0)).expect("cranelift run_main"));
    assert_eq!(cl_v, want, "cranelift const List<String> return mismatch");
    let llvm_v = result_of(&llvm.run_main(arg_i(0)).expect("llvm run_main"));
    assert_eq!(llvm_v, want, "llvm const List<String> return mismatch");
    assert_eq!(cl_v, llvm_v, "cranelift vs llvm const return divergence");
}
