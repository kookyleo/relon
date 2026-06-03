//! P2 G2 — `List<Int>` param → `List<Int>` return (identity `xs -> xs`).
//!
//! Pins the cross-arena copy fix: a `#main(List<Int> xs) -> List<Int> = xs`
//! source lowers to `LoadListIntPtr { 0 } ; StoreField { 0, ListInt } ;
//! Return`. The pointer the input slot carries is *input-buffer-relative*
//! (relative to `in_ptr`), not arena-relative. The frozen codegen used to
//! feed that raw offset straight into the tail-record copy
//! (`emit_store_field_pointer_indirect`), which resolves the source as
//! `arena_base + offset` — reading the wrong arena region and emitting
//! garbage. The fix rebases a param-derived pointer by `in_ptr` before the
//! copy so the record is read from the input region and copied into the
//! output tail.
//!
//! Three-way diff against the tree-walk gold standard (`xs -> xs` is the
//! identity, so the output must equal the input element-for-element).

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// `#main(List<Int> xs) -> List<Int>`: list-param identity.
const LIST_PARAM_SRC: &str = "#main(List<Int> xs) -> List<Int>\nxs\n";

/// Pull a `Vec<i64>` out of a `Value::List` of `Value::Int`.
fn as_int_list(v: &Value) -> Vec<i64> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|e| match e {
                Value::Int(n) => *n,
                other => panic!("expected Int list element, got {other:?}"),
            })
            .collect(),
        other => panic!("expected List result, got {other:?}"),
    }
}

/// Tree-walk gold standard for `src` on the given arg map.
fn oracle(src: &str, args: HashMap<String, Value>) -> Vec<i64> {
    let node = parse_document(src).expect("parse src");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope = Arc::new(Scope::default());
    as_int_list(&walker.run_main(&scope, args).expect("tree-walker run_main"))
}

fn int_list_arg(items: &[i64]) -> HashMap<String, Value> {
    let mut a = HashMap::new();
    a.insert(
        "xs".to_string(),
        Value::list(items.iter().copied().map(Value::Int).collect()),
    );
    a
}

/// Returning a `List<Int>` *parameter* by identity must copy the input
/// record across the input/output arenas, not write the raw input-relative
/// pointer into the output slot. Cross-checked against the tree-walk
/// oracle over a spread of list shapes (including empty + single-element).
#[test]
fn list_param_identity_return_value_three_way() {
    let llvm = LlvmAotEvaluator::from_source(LIST_PARAM_SRC)
        .unwrap_or_else(|e| panic!("LLVM from_source: {e:?}"));

    for items in [
        vec![1_i64, 2, 3],
        vec![],
        vec![42],
        vec![-7, 0, 7, 100, -100],
        vec![i64::MIN, i64::MAX, 0],
    ] {
        let args = int_list_arg(&items);

        let want = oracle(LIST_PARAM_SRC, args.clone());
        assert_eq!(want, items, "oracle sanity for {items:?}");

        let got_llvm = as_int_list(&llvm.run_main(args).expect("llvm run_main"));
        assert_eq!(
            got_llvm, want,
            "LLVM List<Int> param-identity return diverged for {items:?}"
        );
    }
}
