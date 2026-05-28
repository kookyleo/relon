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
///
/// v6-fix-D2 default-path: this source is intentionally non-trivial
/// by the `is_trivial_scalar_main` classifier (the body is an
/// `Expr::FnCall` into a stdlib helper). The trivial classifier
/// short-circuits straight to the tree-walker without ever
/// constructing the AOT slot, which would break the
/// `run_main_routes_through_aot_and_caches` assertion below. Tests
/// that *want* to exercise the trivial short-circuit inline a
/// W11-shaped `#main(Int x) -> Int\nx + 1` source directly.
const MAIN_SOURCE: &str = "#main(Int x) -> Int\nabs(x) * 2";

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
    //
    // The cranelift-AOT backend itself is `&self` but installs the
    // per-call arena pointer onto a shared `SandboxState`; the
    // v5-beta-1 contract documented in `AotEvaluator`
    // requires the host to serialise concurrent `run_main` calls
    // via `Mutex<...>`. We do that here so the test reflects the
    // supported usage pattern — the OnceLock invariant is still
    // validated because the build path races freely before the
    // first call enters the lock.
    use std::sync::Mutex;
    let evaluator = Arc::new(AutoEvaluator::new(MAIN_SOURCE).expect("auto build"));
    let call_gate: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let evaluator = Arc::clone(&evaluator);
            let call_gate = Arc::clone(&call_gate);
            std::thread::spawn(move || {
                let mut args = HashMap::new();
                args.insert("x".to_string(), Value::Int(i));
                let _guard = call_gate.lock().expect("call gate poisoned");
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
        body: Arc::new(node),
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

// =====================================================================
// v6-fix-D2 default-path coverage
// =====================================================================
//
// Path (a): cranelift-AOT cache-hit fast restore. Drive `run_main`
// twice through `Backend::Auto`; the first pass primes the on-disk
// cache via `from_source_with_cache`, the second pass exercises
// `from_cache_dir`'s dlopen-execute path. Both passes must agree on
// the produced `Value`; the cache directory is isolated to a
// `tempfile::TempDir` so concurrent test threads can't interfere.
//
// Path (b): trivial-`#main` tree-walker short-circuit. When the
// source is a single scalar parameter + trivial body, `AutoEvaluator`
// must (1) classify it as trivial at construction time, (2) route
// `run_main` straight through the tree-walker without ever
// constructing the AOT slot, (3) still produce the byte-identical
// `Value` the AOT path would.

/// Default-mode cache fast-path: drive the same source through two
/// fresh `AutoEvaluator`s in the same process, with the on-disk
/// cache redirected to an isolated tempdir. The second `run_main`
/// must hit the dlopen-execute fast restore (we observe this
/// indirectly by checking that the answer is reproduced and the
/// cache directory is populated).
#[cfg(feature = "cranelift-aot")]
#[test]
fn default_path_uses_disk_cache_on_second_call() {
    use std::ffi::OsString;
    use std::path::Path;

    /// Restore the previous `XDG_CACHE_HOME` / `HOME` on drop so
    /// the test stays hermetic even when a later test inspects
    /// these env vars. Safety: this binary runs single-threaded by
    /// default (cargo test serialises tests within a binary unless
    /// `--test-threads` says otherwise); other tests in this file
    /// do not consult `XDG_CACHE_HOME`.
    struct EnvGuard {
        prev_xdg: Option<OsString>,
        prev_home: Option<OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev_xdg.take() {
                    Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                    None => std::env::remove_var("XDG_CACHE_HOME"),
                }
                match self.prev_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    let cache_root = tempfile::tempdir().expect("tempdir for cache");
    let prev_xdg = std::env::var_os("XDG_CACHE_HOME");
    let prev_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", cache_root.path());
        std::env::set_var("HOME", cache_root.path());
    }
    let _guard = EnvGuard {
        prev_xdg,
        prev_home,
    };

    // Non-trivial source so the trivial-`#main` short-circuit (path
    // b) doesn't kick in; we want to observe the AOT cache path.
    let src = "#main(Int x) -> Int\nabs(x) * 3";
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));

    // First pass primes the cache pair (object + ir + schema).
    let first = AutoEvaluator::new(src).expect("first build");
    let r1 = first.run_main(args.clone()).expect("first run_main");
    assert_eq!(r1, Value::Int(21));
    assert!(first.is_aot_initialised());
    drop(first);

    // The cache directory should now contain at least one cache
    // file (object / ir / schema triple). We don't bind to the
    // exact file names so the test stays robust against
    // `relon_codegen_cranelift`'s internal renaming.
    let cache_dir = cache_root.path().join("relon");
    let entries: Vec<_> = std::fs::read_dir(&cache_dir)
        .expect("cache dir populated")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        !entries.is_empty(),
        "first run_main must populate the on-disk cache at {}",
        cache_dir.display()
    );

    // Second pass exercises the `from_cache_dir` dlopen-execute
    // path. We can't directly assert "no cranelift codegen ran"
    // from out here (the cranelift JIT module is hidden behind
    // `Box<dyn Evaluator>`), but reproducing the same `Value` end-
    // to-end is the load-bearing contract.
    let second = AutoEvaluator::new(src).expect("second build");
    let r2 = second.run_main(args).expect("second run_main");
    assert_eq!(r2, Value::Int(21));
    assert_eq!(r1, r2, "cache hit must reproduce the cold answer");
    // The cache dir wasn't deleted between calls.
    assert!(Path::new(&cache_dir).exists());
}

/// Default-path trivial short-circuit: an `AutoEvaluator` built over
/// a `#main(Int x) -> Int\nx + 1` source must classify the body as
/// trivial and route `run_main` through the tree-walker without ever
/// hydrating the cranelift-AOT slot.
#[test]
fn default_path_skips_aot_for_trivial_main() {
    let evaluator = AutoEvaluator::new("#main(Int x) -> Int\nx + 1").expect("auto build");
    // Classifier must accept the W11 shape.
    assert!(
        evaluator.is_trivial_main(),
        "the W11 source must be classified as trivial scalar #main"
    );
    assert!(!evaluator.is_aot_initialised());

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(41));
    let out = evaluator.run_main(args).expect("run_main");
    assert_eq!(out, Value::Int(42));
    // The trivial short-circuit must not have built the AOT slot
    // — that's the entire point of the optimisation.
    assert!(
        !evaluator.is_aot_initialised(),
        "trivial-main classifier must skip cranelift-AOT init"
    );
}

/// Cross-backend parity for a trivial source. The trivial-main
/// short-circuit and the tree-walker path must produce the same
/// `Value`, and that `Value` must also match what an explicit
/// `Backend::TreeWalk` returns. (The cranelift-AOT path can be
/// validated separately via the explicit backend selector below.)
#[test]
fn trivial_source_parity_default_vs_tree_walk() {
    let src = "#main(Int x, Int y) -> Int\nx * y + 7";
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(6));
    args.insert("y".to_string(), Value::Int(7));

    let auto = new_evaluator(src, Backend::Auto).expect("auto build");
    let tw = new_evaluator(src, Backend::TreeWalk).expect("tree-walk build");
    let auto_out = auto.run_main(args.clone()).expect("auto run_main");
    let tw_out = tw.run_main(args).expect("tree-walk run_main");
    assert_eq!(auto_out, Value::Int(49));
    assert_eq!(
        auto_out, tw_out,
        "trivial-main short-circuit must produce byte-identical output to tree-walk"
    );
}

/// Negative-case classifier coverage: a source the trivial
/// classifier must reject (`List<Int>` parameter, closure body)
/// still goes through the AOT slot when `run_main` is invoked. We
/// observe this by checking `is_trivial_main` and
/// `is_aot_initialised` after the call.
#[test]
fn non_trivial_source_still_routes_through_aot_path() {
    // List-typed parameter: definitely not scalar.
    let src = "#main(List<Int> xs) -> Int\n42";
    let evaluator = AutoEvaluator::new(src).expect("auto build");
    assert!(
        !evaluator.is_trivial_main(),
        "List<Int> parameter must disqualify the trivial classifier"
    );

    // Source has no `Op::CallClosure` so the AOT path may or may
    // not accept this exact shape today; whether AOT succeeds or
    // surfaces `Unsupported`, the contract is "the trivial short-
    // circuit did not engage". We assert that bit via
    // `is_aot_initialised` (true after the AOT path was driven,
    // success or failure — both populate the OnceLock slot).
    let mut args = HashMap::new();
    args.insert(
        "xs".to_string(),
        Value::List(Arc::new(vec![Value::Int(1), Value::Int(2)])),
    );
    let _ = evaluator.run_main(args);
    assert!(
        evaluator.is_aot_initialised(),
        "non-trivial source must drive the AOT slot (success or cached failure)"
    );
}
