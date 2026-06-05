//! AutoEvaluator + cranelift-AOT integration smoke.
//!
//! v5-β-2 stage 4: wasm-AOT retired here. The auto-tier wrapper now
//! routes `run_main` through cranelift-AOT exclusively; this file
//! locks down the routing contract for the cranelift path.

#![cfg(feature = "cranelift-aot")]

use std::collections::HashMap;

use relon::{new_evaluator, Backend};
use relon_eval_api::Value;

#[test]
fn auto_backend_runs_simple_arith_through_cranelift() {
    // The cranelift backend handles this shape directly after v5-β-2.
    let src = "#main(Int n) -> Int\nn + 1";
    let evaluator = new_evaluator(src, Backend::Auto).expect("Auto backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(41));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn auto_falls_back_to_tree_walk_on_unsupported_main_shape() {
    // `-> List<P>` is a shape the compiled (cranelift-AOT) backend
    // can't lower today (loud cap: "unsupported type in #main"). Auto
    // must adapt by falling back to the tree-walk oracle and producing
    // the *same* result, not surfacing a setup error to the host.
    //
    // The assertion is "auto == tree-walk" rather than a hard-coded
    // value, so the test keeps passing if the compiled backend later
    // grows support for this shape (the answer is identical either way).
    let src = "#schema P { x: Int }\n#main() -> List<P>\n[]";

    let auto = new_evaluator(src, Backend::Auto).expect("Auto backend");
    let auto_result = auto
        .run_main(HashMap::new())
        .expect("auto run_main should adapt via tree-walk fallback, not error");

    let tree_walk = new_evaluator(src, Backend::TreeWalk).expect("TreeWalk backend");
    let tree_walk_result = tree_walk
        .run_main(HashMap::new())
        .expect("tree-walk run_main");

    assert_eq!(
        auto_result, tree_walk_result,
        "auto fallback output must match the tree-walk oracle"
    );
}

#[test]
fn auto_does_not_swallow_genuine_source_errors() {
    // A real source error (return type mismatch) is NOT a compiled-
    // backend capability boundary. Auto must surface it — never mask it
    // behind a silent tree-walk run — so the user sees the real problem.
    // The tree-walk oracle rejects the same program, confirming this is
    // a genuine error and not a shape the compiled path merely can't
    // express.
    let src = "#main() -> Int\n\"not an int\"";

    let auto = new_evaluator(src, Backend::Auto).expect("Auto backend");
    assert!(
        auto.run_main(HashMap::new()).is_err(),
        "auto must surface a genuine source error, not swallow it via fallback"
    );

    let tree_walk = new_evaluator(src, Backend::TreeWalk).expect("TreeWalk backend");
    assert!(
        tree_walk.run_main(HashMap::new()).is_err(),
        "tree-walk oracle also rejects the program (confirms it is a real error)"
    );
}

#[test]
fn cranelift_backend_runs_simple_arith_directly() {
    // Backend::CraneliftAot bypasses the auto-tier fallback. v5-β-2
    // wired buffer-protocol IR through the cranelift codegen, so this
    // returns a working evaluator and `run_main` produces the same
    // answer as the tree-walker.
    let src = "#main(Int n) -> Int\nn + 1";
    let evaluator = new_evaluator(src, Backend::CraneliftAot).expect("CraneliftAot backend");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(41));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}
