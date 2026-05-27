//! Scope-cut smoke: every cmp_lua workload outside Z.1 routes through
//! the tree-walker fallback. The test confirms (a) construction does
//! NOT fail, (b) `active_tier() == TreeWalker`, (c) run_main returns
//! the same value the tree-walker would.
//!
//! This is the honest panel-row guarantee: a Z.1 scope-cut surfaces
//! as a visible "tree-walker fallback" tier, not a silent pass.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

const W2_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Int\n\
                       list.sum(range(n).map((i) => (i + 1) * (i + 2)))";

#[test]
fn w2_scope_cut_routes_through_tree_walker() {
    let ev = WasmEvaluator::new(W2_SRC).expect("WasmEvaluator::new(W2) — scope-cut OK");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W2 must surface a tree-walker fallback tier (Z.3 follow-up)"
    );
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let out = ev
        .run_main(args)
        .expect("run_main(W2, n=10) via tree-walker");
    // sum((i+1)*(i+2)) for i in 0..10 = 1*2 + 2*3 + ... + 10*11 = 440
    let expected: i64 = (0..10).map(|i| (i + 1) * (i + 2)).sum();
    assert_eq!(out, Value::Int(expected));
}
