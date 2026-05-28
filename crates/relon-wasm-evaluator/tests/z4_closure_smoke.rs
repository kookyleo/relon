//! Phase Z.4.3 — closure-as-value smoke. The IR walker's
//! `MakeClosure` / `CallClosure` lowering wires a funcref table +
//! `call_indirect` dispatch shape so `#internal fib: (k) => ...`-style
//! first-class recursive closures (the W7 production source's
//! pattern) reach `Tier::Compiled` instead of routing through the
//! tree-walker fallback.
//!
//! The smoke pins the round-trip for:
//!
//!  * a minimal closure call inside an anon Dict-return body (single
//!    lambda, no self-recursion — exercises the basic `MakeClosure`
//!    handle alloc + `call_indirect` lookup path), and
//!  * the W7 production source itself (single self-recursive lambda
//!    inside an anon Dict return — the canonical Z.4.3 unlock).
//!
//! Honesty (design §7):
//!
//! 1. Same algorithm? — the wasm module reflects the IR's
//!    `MakeClosure` + `CallClosure` ops one-for-one. No iterative
//!    rewrite, no closed-form substitution. Each recursive `fib(k)`
//!    consumes one wasm stack frame.
//! 2. Same code path? — `WasmEvaluator::new` lowers via
//!    `relon-codegen-wasm`'s IR walker. The Dict-return shape forces
//!    the `IrWalker(DictRecord)` provenance (no fast-path); the
//!    return goes through the host-side schema-layout decode into
//!    `Value::Dict`.
//! 3. Same I/O shape? — input is the `n: Int` arg from a typed-func
//!    `(i64) -> i64` ABI, output is `Value::Dict { result: Int(...) }`
//!    matching the tree-walker reference end-to-end.

use std::collections::HashMap;

use relon_eval_api::{Evaluator, Value};
use relon_wasm_evaluator::{Tier, WasmEvaluator};

/// Tree-walker reference fib (matching the closure body exactly).
fn expected_fib(k: i64) -> i64 {
    if k < 2 {
        k
    } else {
        expected_fib(k - 1) + expected_fib(k - 2)
    }
}

#[test]
fn walker_lowers_simple_closure_in_dict_return() {
    // Minimum-viable closure-as-value shape: one non-recursive
    // closure bound inside an anon Dict-return body. Exercises the
    // captures-free path: `MakeClosure { captures_size: 0 }`
    // produces a handle whose `captures_ptr` slot is zero; the
    // matching `CallClosure` still goes through the funcref table.
    //
    // Source: `result: ((k) => k + 1)(n)` desugars to a single
    // lambda whose only free variable is `n`-or-equivalent (which
    // is passed in as the lambda arg `k`, not captured). Note the
    // IR pipeline may or may not lift this as a true Dict body;
    // we route through the production-style internal-binding form
    // for stability.
    let src = "#main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 inc: (k) => k + 1,\n\
                 result: inc(n)\n\
               }";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(simple closure)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(41));
    let out = ev.run_main(args).expect("run_main(simple closure, n=41)");
    let dict_map = match &out {
        Value::Dict(d) => &d.map,
        other => panic!("expected Value::Dict, got {other:?}"),
    };
    assert_eq!(
        dict_map.get("result").cloned(),
        Some(Value::Int(42)),
        "(k => k + 1)(41) should be 42"
    );
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "simple closure Dict source must reach Tier::Compiled via Z.4.3 IR walker"
    );
}

#[test]
fn walker_lowers_w7_production_self_recursive_fib() {
    // Z.4.3 canonical unlock: the W7 production source. Self-
    // recursive closure captured into a Dict field, called via
    // `result: fib(n)`. The matching IR shape is:
    //
    //   * outer entry: `AllocRootRecord` + `MakeClosure { fn_table_idx:
    //     0, captures: [ClosureCapture { let_idx: 0, ty: Closure,
    //     offset: 0 }], captures_size: 8 }` + `LetSet { ty: Closure }`
    //     + `LetGet { ty: Closure }` + `LoadField { ty: I64 }` +
    //     `CallClosure { param_tys: [I64], ret_ty: I64 }` +
    //     `StoreFieldAtRecord` + `Return`.
    //   * lambda `__closure_0`: `(captures_ptr: i32, k: i64) -> i64`
    //     reading its self-handle off the captures struct
    //     (`LocalGet 0` + `LoadI32AtAbsolute { offset: 0 }`) and
    //     recursing through two `CallClosure` ops in the else arm.
    //
    // Pinned at the bench point (`n = 22`) so the smoke pins the
    // bench's expected fib(22) = 17711 end-to-end through the same
    // wasm module the bench's `relon_wasm_wasmtime` row would
    // exercise post-Z.4.3.
    let src = "#main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                 result: fib(n)\n\
               }";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(W7 production)");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(22));
    let out = ev.run_main(args).expect("run_main(W7 production, n=22)");
    let dict_map = match &out {
        Value::Dict(d) => &d.map,
        other => panic!("expected Value::Dict, got {other:?}"),
    };
    assert_eq!(
        dict_map.get("result").cloned(),
        Some(Value::Int(expected_fib(22))),
        "W7 production Dict.result must match the tree-walker fib(22)"
    );
    assert_eq!(
        dict_map.get("result").cloned(),
        Some(Value::Int(17711)),
        "W7 production Dict.result must equal 17711 (fib(22))"
    );
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "W7 production must reach Tier::Compiled via Z.4.3 funcref-table dispatch"
    );
}

#[test]
fn walker_lowers_w7_production_at_small_n() {
    // Same source, smaller n. Pins the base-case predicate (`k < 2`)
    // and the single-recursion arm; a regression on the self-
    // capture path would surface either as a wasm trap or a wrong
    // numeric result at small n.
    let src = "#main(Int n) -> Dict\n\
               {\n\
                 #internal\n\
                 fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2),\n\
                 result: fib(n)\n\
               }";
    let ev = WasmEvaluator::new(src).expect("WasmEvaluator::new(W7 production, small n)");
    for n in 0_i64..=10 {
        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(n));
        let out = ev
            .run_main(args)
            .unwrap_or_else(|e| panic!("run_main(W7 production, n={n}): {e:?}"));
        let dict_map = match &out {
            Value::Dict(d) => &d.map,
            other => panic!("expected Value::Dict at n={n}, got {other:?}"),
        };
        assert_eq!(
            dict_map.get("result").cloned(),
            Some(Value::Int(expected_fib(n))),
            "W7 production Dict.result mismatch at n={n}"
        );
    }
    assert_eq!(
        ev.active_tier(),
        Tier::Compiled,
        "W7 production must stay on Tier::Compiled across the small-n sweep"
    );
}
