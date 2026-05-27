//! Scope-cut smoke: every cmp_lua workload outside Z.1/Z.3 routes through
//! the tree-walker fallback. The test confirms (a) construction does
//! NOT fail, (b) `active_tier() == TreeWalker`, (c) run_main returns
//! the same value the tree-walker would.
//!
//! This is the honest panel-row guarantee: a scope-cut surfaces
//! as a visible "tree-walker fallback" tier, not a silent pass.
//!
//! Promotion history:
//! - W2: Z.3c-a — `range.map.sum` inline (see `w2_smoke.rs`).
//! - W3: Z.3c-b — `range.map.reduce` inline (see `w3_smoke.rs`).
//! - W10 inline: Z.3c-b — inline-Int access-control loop (see
//!   `w10_smoke.rs`).
//!
//! W5 (dict access) is the current scope-cut stand-in because the
//! dict-literal + `i % 10` indexing surface is still on the Z.4
//! follow-up queue. Source is duplicated byte-identical from
//! `w5_relon_src()` in `crates/relon-bench/benches/cmp_lua.rs`.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

// W5 — dict access. Stays scope-cut until the dict-literal + str-key
// indexing surface lands. Production source returns `Dict`, so the
// run-through-tree-walker check looks at the wrapped `.result`.
const W5_SRC: &str = "#import list from \"std/list\"\n\
                       #main(Int n) -> Dict\n\
                       {\n\
                         #internal\n\
                         d: { a: 1, b: 2, c: 3, d: 4, e: 5, f: 6, g: 7, h: 8, i: 9, j: 10 },\n\
                         #internal\n\
                         keys: [\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\", \"j\"],\n\
                         result: list.sum(range(n).map((i) => d[keys[i % 10]]))\n\
                       }";

#[test]
fn w5_scope_cut_routes_through_tree_walker() {
    let ev = WasmEvaluator::new(W5_SRC).expect("WasmEvaluator::new(W5) — scope-cut OK");
    assert_eq!(
        ev.active_tier(),
        Tier::TreeWalker,
        "W5 must surface a tree-walker fallback tier (Z.4 follow-up)"
    );
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let out = ev
        .run_main(args)
        .expect("run_main(W5, n=10) via tree-walker");
    // n=10 visits each key exactly once, summing 1..=10 = 55.
    // The dict-bodied return surfaces as a Dict whose `result`
    // field is the sum.
    match out {
        Value::Dict(d) => {
            let result = d.map.get("result").expect("W5 dict missing `result` field");
            assert_eq!(result, &Value::Int(55), "W5 tree-walker result mismatch");
        }
        other => panic!("W5 expected Dict, got {other:?}"),
    }
}
