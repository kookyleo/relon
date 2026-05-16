//! Phase 8 smoke tests for [`relon_codegen_wasm::WasmAotEvaluator`].
//!
//! Drives the wasm-AOT backend end-to-end through the public
//! [`relon_eval_api::Evaluator`] surface, then compares each result
//! against the tree-walker's output so the two backends stay in
//! parity for the v1 leaf-type set.
//!
//! Coverage:
//!
//! * `parity_int_doubling` — primitive Int → Int parity.
//! * `parity_string_passthrough` — pointer-indirect String result.
//! * `parity_dict_literal_return` — branded user-schema return path.
//! * `division_by_zero_matches_tree_walker` — trap → RuntimeError
//!   parity for the `x / 0` shape.
//! * `eval_returns_unsupported` — `eval` / `eval_root` /
//!   `force_thunk` / `invoke_closure` all return Unsupported.
//! * `run_main_missing_argument_errors` — the buffer builder
//!   surfaces a `MissingMainArg` before the wasm side ever runs.

use relon_codegen_wasm::WasmAotEvaluator;
use relon_eval_api::{Evaluator, RuntimeError, Scope, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// Compile `source` for tree-walk evaluation. Returns a closure
/// the test can drive with `run_main` so we don't repeat the
/// analyzer / evaluator setup across cases.
fn tree_walk_run(source: &str, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
    use relon_eval_api::Context;
    use relon_evaluator::TreeWalkEvaluator;
    let ast = relon_parser::parse_document(source).expect("parse");
    let analyzed = relon_analyzer::analyze(&ast);
    assert!(
        !analyzed.has_errors(),
        "tree-walk analyzer errors: {:?}",
        analyzed.diagnostics
    );
    let analyzed = Arc::new(analyzed);
    let mut ctx = Context::new()
        .with_root(ast)
        .with_analyzed(Arc::clone(&analyzed));
    TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let evaluator = TreeWalkEvaluator::new(Arc::new(ctx));
    Evaluator::run_main(&evaluator, args)
}

#[test]
fn parity_int_doubling() {
    let src = "#main(Int x) -> Int\nx * 2";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(21));

    let aot_out = aot.run_main(args.clone()).expect("aot run_main");
    let walker_out = tree_walk_run(src, args).expect("tree-walk run_main");

    assert_eq!(aot_out, Value::Int(42));
    assert_eq!(aot_out, walker_out);
}

#[test]
fn parity_string_passthrough() {
    // String -> String returns through the pointer-indirect tail
    // record. Confirms BufferReader::read_string flows through
    // run_main_inner without truncation or UTF-8 corruption.
    let src = "#main(String s) -> String\ns";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut args = HashMap::new();
    args.insert("s".to_string(), Value::String("hello, wasm".to_string()));

    let aot_out = aot.run_main(args.clone()).expect("aot run_main");
    let walker_out = tree_walk_run(src, args).expect("tree-walk run_main");

    assert_eq!(aot_out, Value::String("hello, wasm".to_string()));
    assert_eq!(aot_out, walker_out);
}

#[test]
fn parity_dict_literal_return() {
    // Branded user-schema return: tree-walker returns a branded
    // Dict; wasm-AOT decodes the same shape via the sub-record
    // read path so the two values compare equal under PartialEq.
    let src = r#"#schema User { Int age: *, String name: * }
#main(Int x) -> User
{ age: x, name: "ada" }
"#;
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(36));

    let aot_out = aot.run_main(args.clone()).expect("aot run_main");
    let walker_out = tree_walk_run(src, args).expect("tree-walk run_main");

    // Extract the dict-shaped payload from each side and compare
    // map contents — branded Dict equality preserves brand.
    match (&aot_out, &walker_out) {
        (Value::Dict(a), Value::Dict(b)) => {
            assert_eq!(a.map.get("age"), b.map.get("age"));
            assert_eq!(a.map.get("name"), b.map.get("name"));
            assert_eq!(a.map.get("age"), Some(&Value::Int(36)));
            assert_eq!(a.map.get("name"), Some(&Value::String("ada".to_string())));
        }
        other => panic!("expected matching Dict pair, got {other:?}"),
    }
}

#[test]
fn division_by_zero_matches_tree_walker() {
    // `x / 0` traps in wasm via `i64.div_s` -> IntegerDivisionByZero;
    // translate_trap maps it to RuntimeError::DivisionByZero. The
    // tree-walker emits the same variant. We compare variant shape
    // (range is naturally different across backends).
    let src = "#main(Int x, Int y) -> Int\nx / y";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(10));
    args.insert("y".to_string(), Value::Int(0));

    let aot_err = aot.run_main(args.clone()).expect_err("aot must error");
    let walker_err = tree_walk_run(src, args).expect_err("tree-walk must error");

    assert!(
        matches!(aot_err, RuntimeError::DivisionByZero(_)),
        "aot: expected DivisionByZero, got {aot_err:?}"
    );
    assert!(
        matches!(walker_err, RuntimeError::DivisionByZero(_)),
        "tree-walk: expected DivisionByZero, got {walker_err:?}"
    );
}

#[test]
fn eval_returns_unsupported() {
    // wasm-AOT cannot evaluate arbitrary AST nodes: there's no
    // AST left after codegen. All four "non-run_main" trait
    // methods must surface Unsupported instead of panicking.
    let src = "#main(Int x) -> Int\nx";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    // We need a Node to test eval; reach into the parser directly
    // since the evaluator deliberately doesn't expose its AST.
    let node = relon_parser::parse_document("{ x: 1 }").expect("parse aux");
    let scope = Arc::new(Scope::default());

    let err = Evaluator::eval(&aot, &node, &scope).expect_err("must be unsupported");
    assert!(matches!(err, RuntimeError::Unsupported { .. }));

    let err = Evaluator::eval_root(&aot, &scope).expect_err("must be unsupported");
    assert!(matches!(err, RuntimeError::Unsupported { .. }));
}

#[test]
fn force_thunk_returns_unsupported() {
    // Like eval/eval_root: no live thunks under topo-eager AOT.
    use relon_eval_api::Thunk;
    let src = "#main(Int x) -> Int\nx";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    // Build a minimal thunk via the type's public surface. The
    // wasm-AOT impl never inspects it — Unsupported short-circuits
    // before any field access.
    let node = relon_parser::parse_document("1").expect("parse thunk node");
    let thunk = Arc::new(Thunk::new(
        node,
        Arc::new(Scope::default()),
        Vec::new(),
        String::new(),
    ));

    let err = Evaluator::force_thunk(&aot, &thunk).expect_err("must be unsupported");
    assert!(matches!(err, RuntimeError::Unsupported { .. }));
}

#[test]
fn invoke_closure_returns_unsupported() {
    use relon_eval_api::ClosureData;
    let src = "#main(Int x) -> Int\nx";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let body = relon_parser::parse_document("0").expect("parse body");
    let closure = ClosureData {
        params: vec![],
        body,
        captured_env: Arc::new(Scope::default()),
    };

    let err = Evaluator::invoke_closure(&aot, &closure, &[]).expect_err("must be unsupported");
    assert!(matches!(err, RuntimeError::Unsupported { .. }));
}

#[test]
fn run_main_missing_argument_errors() {
    // The buffer builder needs every declared #main param. We
    // surface `MissingMainArg` before the wasm side runs.
    let src = "#main(Int x) -> Int\nx";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let err = aot.run_main(HashMap::new()).expect_err("must error");
    match err {
        RuntimeError::MissingMainArg { name, .. } => {
            assert_eq!(name, "x");
        }
        other => panic!("expected MissingMainArg, got {other:?}"),
    }
}
