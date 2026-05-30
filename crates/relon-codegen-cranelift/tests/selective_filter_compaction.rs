//! Regression for WORK ITEM #357: a *selective* `_list_filter`
//! (a predicate that DROPS some elements) returned an empty list
//! through the cranelift AOT backend, so `list.sum(_list_filter(...))`
//! yielded 0 for all `n`. A keep-everything filter and the plain
//! materialise+reduce path were both correct, isolating the defect to
//! the filter's per-element conditional keep (the survivor write + the
//! output-length cursor) in the cranelift lowering of the bundled
//! `list_int_filter` stdlib body. The LLVM AOT backend handles the same
//! body correctly.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_cranelift::AotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;

/// `list.sum(_list_filter(xs, (v) => v < K))` over a materialised
/// `range(n).map(..)` list. `K` controls the drop fraction:
///   * very large K  -> keep everything
///   * mid K         -> drop ~half
///   * K = 0         -> drop everything
fn filter_sum_src(k: i64) -> String {
    format!(
        "#unstrict\n\
         #import list from \"std/list\"\n\
         #main(Int n) -> Int\n\
         list.sum(_list_filter(xs, (v) => v < {k}))\n\
         where {{\n\
           xs: range(n).map((i) => (i * 1103515245 + 12345) % 2048)\n\
         }}"
    )
}

fn oracle(src: &str, n: i64) -> i64 {
    let node = parse_document(src).expect("oracle parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let walker = TreeWalkEvaluator::new(Arc::new(ctx));
    let scope: Arc<Scope> = Arc::new(Scope::default());
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match walker.run_main(&scope, args).expect("oracle run_main") {
        Value::Int(v) => v,
        other => panic!("oracle returned non-int: {other:?}"),
    }
}

fn aot_run(src: &str, n: i64) -> i64 {
    let eval = AotEvaluator::from_source(src).expect("filter-shape AOT must compile");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(n));
    match eval.run_main(args).expect("filter-shape run_main") {
        Value::Int(v) => v,
        other => panic!("AOT returned non-int: {other:?}"),
    }
}

/// The element values are `(i * 1103515245 + 12345) % 2048`, i.e. in
/// `[0, 2048)`. Picking `K` across that range gives drop-none (K=2048),
/// drop-~half (K=1000), and drop-all (K=0) without depending on the
/// exact PRNG distribution — the oracle pins the true answer either way.
#[test]
fn selective_filter_sum_matches_oracle() {
    for k in [2048_i64, 1000, 0] {
        let src = filter_sum_src(k);
        for n in [0_i64, 1, 4, 17, 100, 513] {
            let want = oracle(&src, n);
            let got = aot_run(&src, n);
            assert_eq!(
                got, want,
                "selective filter (v < {k}) sum mismatch at n={n}: aot={got} oracle={want}"
            );
        }
    }
}
