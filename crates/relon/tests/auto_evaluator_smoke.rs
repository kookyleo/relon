//! Smoke tests for [`relon::AutoEvaluator`] and `Backend::Auto`.
//!
//! v5-β-2 stage 4: wasm-AOT retired; the AOT path is cranelift only.
//! The routing contract this file covers:
//!
//! * Default backend is `Auto`.
//! * Tree-walker-only methods (`eval`, `eval_root`, `force_thunk`,
//!   `invoke_closure`) succeed without ever constructing the
//!   cranelift-AOT backend.
//! * `run_main` routes through cranelift-AOT and produces the same
//!   value as the tree-walker.
//! * `run_main` failure does not poison `eval` / `eval_root`.
//! * Concurrent `run_main` calls share one AOT build via `OnceLock`.
//! * `Backend::Auto` and `Backend::default()` agree.
//! * The CLI `BackendArg::Auto` default reaches the library via the
//!   `new_evaluator(_, Backend::default())` path.

use std::collections::HashMap;
use std::sync::Arc;

use relon::{new_evaluator, AutoEvaluator, Backend};
use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};

/// A library-mode (no `#main`) script the tree-walker can drive but
/// the wasm-AOT backend rejects. Useful for verifying `eval_root` /
/// `eval` never touch the AOT pipeline.
const LIB_SOURCE: &str = r#"{ host: "localhost", port: 8080 }"#;

/// A `#main(...)` entry script that both backends accept. Returns
/// `x * 2` so `run_main(args={x:21})` produces `Value::Int(42)`.
const MAIN_SOURCE: &str = "#main(Int x) -> Int\nx * 2";

/// A `#main(...)` shape the cranelift-AOT backend currently rejects
/// (closure-bearing higher-order list op — `Op::CallClosure` is
/// Phase C.4 deferred) while the tree-walker keeps working.
/// Exercises the failure-isolation invariant for the AOT init slot.
const AOT_REJECTED_MAIN: &str = "#main(List<Int> xs) -> List<Int>\nxs.map((Int n) => n * 2)";

#[test]
fn backend_default_is_auto() {
    // Locking the `#[default]` choice down so tooling / hosts that
    // rely on `Backend::default()` get the v4-e auto-tier wrapper
    // rather than the plain tree-walker.
    assert_eq!(Backend::default(), Backend::Auto);
}

#[test]
fn new_evaluator_auto_returns_auto_evaluator() {
    // Smoke-check the public factory wires through `AutoEvaluator`.
    // We can't downcast a `Box<dyn Evaluator>` to a concrete type
    // here, but we can confirm the eval surface works.
    let evaluator = new_evaluator(LIB_SOURCE, Backend::Auto).expect("auto build");
    let scope = Arc::new(Scope::default());
    let out = evaluator.eval_root(&scope).expect("eval_root");
    match out {
        Value::Dict(d) => {
            assert_eq!(d.map.get("host"), Some(&Value::String("localhost".into())));
            assert_eq!(d.map.get("port"), Some(&Value::Int(8080)));
        }
        other => panic!("expected Dict, got {other:?}"),
    }
}

#[test]
fn lazy_aot_init_skipped_for_eval_root() {
    // Pure library-mode flow: a host that only reads static config
    // should never trigger the wasm-AOT pipeline. The flag exposes
    // the OnceLock state for the test.
    let evaluator = AutoEvaluator::new(LIB_SOURCE).expect("auto build");
    assert!(!evaluator.is_aot_initialised(), "should start lazy");
    let scope = Arc::new(Scope::default());
    let _ = evaluator.eval_root(&scope).expect("eval_root succeeds");
    assert!(
        !evaluator.is_aot_initialised(),
        "eval_root must not trigger AOT init"
    );
}

#[test]
fn lazy_aot_init_skipped_for_eval_on_arbitrary_node() {
    // `eval` on a literal node: also must stay tree-walk-only.
    let evaluator = AutoEvaluator::new(MAIN_SOURCE).expect("auto build");
    let aux_node = relon_parser::parse_document("42").expect("parse aux");
    let scope = Arc::new(Scope::default());
    let out = evaluator.eval(&aux_node, &scope).expect("eval succeeds");
    assert_eq!(out, Value::Int(42));
    assert!(
        !evaluator.is_aot_initialised(),
        "eval must not trigger AOT init"
    );
}

#[test]
fn run_main_routes_through_aot_and_caches() {
    // First `run_main` call must spin up the AOT backend, produce the
    // expected value, and flip `is_aot_initialised` to true. A second
    // call must reuse the cached AOT — implicit through the OnceLock
    // contract, but we still assert the result stays consistent.
    let evaluator = AutoEvaluator::new(MAIN_SOURCE).expect("auto build");
    assert!(!evaluator.is_aot_initialised());

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(21));
    let out1 = evaluator.run_main(args.clone()).expect("first run_main");
    assert_eq!(out1, Value::Int(42));
    assert!(
        evaluator.is_aot_initialised(),
        "run_main must hydrate AOT slot"
    );

    let out2 = evaluator.run_main(args).expect("second run_main");
    assert_eq!(out2, Value::Int(42));
}

#[test]
fn aot_init_failure_does_not_poison_tree_walk_surface() {
    // When the AOT pipeline rejects the source, only `run_main`
    // should surface the error; `eval` / `eval_root` / `force_thunk`
    // / `invoke_closure` must keep working off the tree-walker.
    let evaluator = AutoEvaluator::new(AOT_REJECTED_MAIN).expect("auto build");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));

    // First `run_main` triggers the AOT pipeline and fails because
    // closure-typed returns are outside the cranelift AOT envelope
    // today (Phase C.4 deferred work).
    let err = evaluator
        .run_main(args.clone())
        .expect_err("AOT rejects closure-typed return today");
    assert!(matches!(err, RuntimeError::Unsupported { .. }));
    assert!(
        evaluator.is_aot_initialised(),
        "failure path must mark the slot to avoid retrying"
    );

    // Tree-walk-side methods are unaffected: pull the aux node and
    // confirm `eval` still works after the AOT failure was cached.
    let aux_node = relon_parser::parse_document("1 + 2").expect("parse aux");
    let scope = Arc::new(Scope::default());
    let out = evaluator
        .eval(&aux_node, &scope)
        .expect("tree-walk eval still works after AOT failure");
    assert_eq!(out, Value::Int(3));

    // Second `run_main` returns the cached error — same shape, no
    // re-entry into the pipeline.
    let err2 = evaluator.run_main(args).expect_err("cached AOT failure");
    assert!(matches!(err2, RuntimeError::Unsupported { .. }));
}

#[test]
fn explicit_backend_treewalk_skips_auto_wrapper() {
    // `Backend::TreeWalk` must keep producing a raw tree-walker (no
    // AOT slot at all). We can't downcast a trait object, but we
    // can confirm `eval` / `eval_root` succeed and the type isn't
    // an `AutoEvaluator` by reproducing the path with the explicit
    // tree-walker route.
    let evaluator = new_evaluator(MAIN_SOURCE, Backend::TreeWalk).expect("tree-walk build");
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(2));
    let out = evaluator.run_main(args).expect("tree-walk run_main");
    assert_eq!(out, Value::Int(4));
}

#[cfg(feature = "cranelift-aot")]
#[test]
fn run_main_value_parity_auto_vs_tree_walk() {
    // `Auto::run_main` (via cranelift AOT) and `TreeWalk::run_main`
    // must agree on the exact `Value` for the Int-leaf matrix the
    // cranelift backend covers today. String pass-through
    // (`return s`) needs `Op::LoadStringPtr`, which is deferred to
    // v5-γ along with `Op::CallNative` / `Op::CallClosure` — so we
    // restrict the parity cases to Int / arithmetic shapes the
    // current AOT envelope handles.
    let cases: &[(&str, HashMap<String, Value>, Value)] = &[
        (
            "#main(Int x) -> Int\nx * 3",
            {
                let mut m = HashMap::new();
                m.insert("x".into(), Value::Int(7));
                m
            },
            Value::Int(21),
        ),
        (
            "#main(Int x, Int y) -> Int\nx + y",
            {
                let mut m = HashMap::new();
                m.insert("x".into(), Value::Int(40));
                m.insert("y".into(), Value::Int(2));
                m
            },
            Value::Int(42),
        ),
    ];

    for (src, args, expected) in cases {
        let auto = new_evaluator(src, Backend::Auto).expect("auto build");
        let tw = new_evaluator(src, Backend::TreeWalk).expect("tree-walk build");
        let auto_out = auto.run_main(args.clone()).expect("auto run_main");
        let tw_out = tw.run_main(args.clone()).expect("tree-walk run_main");
        assert_eq!(&auto_out, expected, "auto out mismatch for {src}");
        assert_eq!(&tw_out, expected, "tree-walk out mismatch for {src}");
        assert_eq!(auto_out, tw_out, "backend parity for {src}");
    }
}

#[test]
fn concurrent_run_main_only_builds_aot_once() {
    // Spawn N threads racing on `run_main`. The `OnceLock` slot
    // guarantees the AOT pipeline runs at most once; we observe
    // the invariant indirectly by checking every thread sees the
    // correct value, and `is_aot_initialised()` flips before the
    // join. A backend that re-ran the pipeline per call would
    // still produce the right value, but the test guards against
    // a future refactor that drops the OnceLock and falls back
    // to per-call construction.
    let evaluator = Arc::new(AutoEvaluator::new(MAIN_SOURCE).expect("auto build"));
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let evaluator = Arc::clone(&evaluator);
            std::thread::spawn(move || {
                let mut args = HashMap::new();
                args.insert("x".to_string(), Value::Int(i));
                evaluator.run_main(args).expect("thread run_main")
            })
        })
        .collect();
    for (i, t) in threads.into_iter().enumerate() {
        let out = t.join().expect("thread join");
        assert_eq!(out, Value::Int((i as i64) * 2));
    }
    assert!(evaluator.is_aot_initialised());
}

#[test]
fn force_thunk_and_invoke_closure_route_through_tree_walk() {
    // Build a thunk + closure directly via the eval-api types, then
    // drive them through the auto wrapper. The cranelift-AOT backend
    // returns `Unsupported` for these; `AutoEvaluator` must route
    // through the tree-walker.
    //
    // We can't easily synthesise a *valid* thunk / closure for the
    // tree-walker without an actual eval session (Thunk needs a
    // populated cache_key, ClosureData needs a real captured env).
    // What we *can* guarantee is the routing: the calls must not
    // return `Unsupported { reason: "wasm-aot ..." }`.
    let evaluator = AutoEvaluator::new(LIB_SOURCE).expect("auto build");
    let node = relon_parser::parse_document("1").expect("parse thunk node");
    let thunk = Arc::new(Thunk::new(
        node.clone(),
        Arc::new(Scope::default()),
        Vec::new(),
        String::new(),
    ));
    // The tree-walker may or may not succeed on this synthetic thunk;
    // the only contract we assert here is that whatever error it
    // produces, it isn't the wasm-AOT "Unsupported" sentinel.
    let res = evaluator.force_thunk(&thunk);
    if let Err(RuntimeError::Unsupported { reason }) = &res {
        assert!(
            !reason.contains("cranelift-AOT"),
            "force_thunk must not route through cranelift-AOT; got: {reason}"
        );
    }

    let closure = ClosureData {
        params: vec![],
        body: node,
        captured_env: Arc::new(Scope::default()),
    };
    let res = evaluator.invoke_closure(&closure, &[]);
    if let Err(RuntimeError::Unsupported { reason }) = &res {
        assert!(
            !reason.contains("cranelift-AOT"),
            "invoke_closure must not route through cranelift-AOT; got: {reason}"
        );
    }
    // Even after exercising every tree-walker-only path the AOT
    // slot must remain unbuilt.
    assert!(
        !evaluator.is_aot_initialised(),
        "no tree-walker-only path may trigger AOT init"
    );
}
