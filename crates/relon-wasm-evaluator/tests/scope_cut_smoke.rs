//! Scope-cut smoke: every cmp_lua workload outside Z.1/Z.3 routes through
//! the tree-walker fallback. The test confirms (a) construction does
//! NOT fail, (b) `active_tier() == TreeWalker`, (c) run_main returns
//! the same value the tree-walker would.
//!
//! This is the honest panel-row guarantee: a scope-cut surfaces
//! as a visible "tree-walker fallback" tier, not a silent pass.
//!
//! W2 used to live here; Z.3c-a promoted it to a real WASM lowering
//! (see `w2_smoke.rs`). We pick W3 as the stand-in scope-cut probe
//! because the String concat surface is still on the Z.3+ follow-up
//! queue.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// W3 — string concat. Stays scope-cut until the
// `__relon_str_concat_n` plumbing lands.
const W3_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> String\n\
                       range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";

#[test]
fn w3_scope_cut_routes_through_tree_walker() {
    let ev = WasmEvaluator::new(W3_SRC).expect("WasmEvaluator::new(W3) — scope-cut OK");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W3 must surface a tree-walker fallback tier (Z.3+ follow-up)"
    );
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(4));
    let out = ev
        .run_main(args)
        .expect("run_main(W3, n=4) via tree-walker");
    // 4 copies of "a" concatenated.
    assert_eq!(out, Value::String("aaaa".into()));
}
