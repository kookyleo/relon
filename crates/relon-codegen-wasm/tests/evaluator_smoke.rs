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
fn pool_reuse_repeated_invocations_stay_consistent() {
    // Phase 9.b-1: the WasmAotEvaluator pools warmed wasm sessions
    // across invocations. Run the same source many times and confirm
    // each call returns the canonical value — a pool that leaked
    // stale `in_buf` bytes from a previous call would surface as a
    // wrong-answer regression here.
    let src = "#main(Int x, Int y) -> Int\nx * y + 1";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");
    for i in 0..50 {
        let mut args = HashMap::new();
        args.insert("x".to_string(), Value::Int(i));
        args.insert("y".to_string(), Value::Int(i + 1));
        let out = aot.run_main(args).expect("run_main");
        assert_eq!(out, Value::Int(i * (i + 1) + 1), "iteration {i}");
    }
}

#[test]
fn pool_reuse_after_trap_still_works() {
    // A wasm trap (division by zero here) must not poison the pool —
    // the session goes back so the next call still hits the warm
    // path. Drive a trap followed by a clean call and confirm both
    // resolve as expected.
    let src = "#main(Int x, Int y) -> Int\nx / y";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut bad = HashMap::new();
    bad.insert("x".to_string(), Value::Int(1));
    bad.insert("y".to_string(), Value::Int(0));
    let err = aot.run_main(bad).expect_err("must trap");
    assert!(matches!(err, RuntimeError::DivisionByZero(_)));

    let mut good = HashMap::new();
    good.insert("x".to_string(), Value::Int(10));
    good.insert("y".to_string(), Value::Int(2));
    let out = aot.run_main(good).expect("clean call after trap");
    assert_eq!(out, Value::Int(5));
}

#[test]
fn schema_arg_via_pool() {
    // Phase 9.b-1: WasmAotEvaluator now accepts Schema-typed `#main`
    // arguments through `BufferBuilder::sub_record`. The Dict value
    // mirrors the canonical schema shape — the host passes a
    // `Value::Dict { x: 21 }` for a `#main(V v) -> Int` whose `V` has
    // an `Int x` field plus a `doubled()` method. Tree-walker parity
    // confirms the schema-method dispatch path lights up identically.
    let src = "#schema V { Int x: * } with {\n  \
        doubled() -> Int: self.x * 2\n\
        }\n\
        #main(V v) -> Int\n\
        v.doubled()";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut v_map = std::collections::BTreeMap::new();
    v_map.insert("x".to_string(), Value::Int(21));
    let mut args = HashMap::new();
    args.insert(
        "v".to_string(),
        Value::branded_dict(v_map, Some("V".into())),
    );

    let aot_out = aot.run_main(args.clone()).expect("aot run_main");
    let walker_out = tree_walk_run(src, args).expect("tree-walk run_main");

    assert_eq!(aot_out, Value::Int(42));
    assert_eq!(aot_out, walker_out);
}

#[test]
fn schema_arg_with_two_int_fields_roundtrips() {
    // Sub-record arg with two scalar fields — exercises the pointer
    // slot in MainParams plus the inline-int reads inside the
    // sub-record. The schema-rooted method runs through the existing
    // self.field dispatch path, which is the most heavily covered
    // method_dispatch_smoke shape.
    let src = "#schema Pair { Int a: *, Int b: * } with {\n  \
        sum() -> Int: self.a + self.b\n\
        }\n\
        #main(Pair p) -> Int\n\
        p.sum()";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut p_map = std::collections::BTreeMap::new();
    p_map.insert("a".to_string(), Value::Int(3));
    p_map.insert("b".to_string(), Value::Int(4));
    let mut args = HashMap::new();
    args.insert(
        "p".to_string(),
        Value::branded_dict(p_map, Some("Pair".into())),
    );

    let aot_out = aot.run_main(args.clone()).expect("aot run_main");
    let walker_out = tree_walk_run(src, args).expect("tree-walk run_main");
    assert_eq!(aot_out, Value::Int(7));
    assert_eq!(aot_out, walker_out);
}

#[test]
fn schema_arg_with_string_field() {
    // `self.name.length()` chains a pointer-indirect String load
    // through `LoadFieldAtAbsolute`. The field's 4-byte slot inside
    // the sub-record's fixed area holds a buffer-relative offset; the
    // codegen path must rebase it to absolute by adding `in_ptr` so
    // the stdlib `String::length` body sees a real `[len][bytes]`
    // record. Without the rebase the load walks arbitrary bytes and
    // returns garbage.
    //
    // Tree-walk parity is skipped here: the surface stdlib name for
    // String length is `len` on the walker side and `length` on the
    // wasm-AOT side — bringing those into parity is a separate task.
    let src = "#schema P { Int x: *, String name: * } with {\n  \
        hello() -> Int: self.name.length()\n\
        }\n\
        #main(P p) -> Int\n\
        p.hello()";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut p_map = std::collections::BTreeMap::new();
    p_map.insert("x".to_string(), Value::Int(0));
    p_map.insert("name".to_string(), Value::String("world".to_string()));
    let mut args = HashMap::new();
    args.insert(
        "p".to_string(),
        Value::branded_dict(p_map, Some("P".into())),
    );

    let aot_out = aot.run_main(args).expect("aot run_main");
    assert_eq!(aot_out, Value::Int(5));
}

#[test]
fn schema_arg_with_list_int_field() {
    // Same pointer-indirect rebase contract as `schema_arg_with_
    // string_field`, but for `List<Int>` slots. `self.xs.length()`
    // routes through the `ListInt` arm of `LoadFieldAtAbsolute`; the
    // codegen path must add `in_ptr` to the loaded buffer-relative
    // offset so the stdlib body reads the element count from the
    // real tail record.
    let src = "#schema Q { List<Int> xs: * } with {\n  \
        size() -> Int: self.xs.length()\n\
        }\n\
        #main(Q q) -> Int\n\
        q.size()";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let mut q_map = std::collections::BTreeMap::new();
    q_map.insert(
        "xs".to_string(),
        Value::List(Arc::new(vec![
            Value::Int(10),
            Value::Int(20),
            Value::Int(30),
        ])),
    );
    let mut args = HashMap::new();
    args.insert(
        "q".to_string(),
        Value::branded_dict(q_map, Some("Q".into())),
    );

    let aot_out = aot.run_main(args).expect("aot run_main");
    assert_eq!(aot_out, Value::Int(3));
}

#[test]
fn schema_arg_missing_inner_field_errors() {
    // The buffer builder demands every sub-field; surface a clear
    // MissingMainArg before the wasm side ever runs.
    let src = "#schema V { Int x: * } with {\n  \
        doubled() -> Int: self.x * 2\n\
        }\n\
        #main(V v) -> Int\n\
        v.doubled()";
    let aot = WasmAotEvaluator::from_source(src).expect("compile");

    let v_map = std::collections::BTreeMap::new();
    let mut args = HashMap::new();
    args.insert(
        "v".to_string(),
        Value::branded_dict(v_map, Some("V".into())),
    );

    let err = aot.run_main(args).expect_err("missing inner must error");
    match err {
        RuntimeError::MissingMainArg { name, .. } => assert_eq!(name, "v.x"),
        other => panic!("expected MissingMainArg, got {other:?}"),
    }
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

/// Build a hand-rolled IR module that drops an `Op::CheckCap` (the
/// stand-alone capability assertion) at the start of `run_main`. The
/// body is otherwise trivial: a constant store + return. Used by the
/// `WasmAotEvaluator::with_capabilities` tests below — the IR has no
/// `#native fn` imports so the evaluator's linker can instantiate the
/// module without help, yet the body still trips the `check_cap`
/// prologue path when the host's `Capabilities` lack `ReadsFs`.
fn build_check_cap_module(cap_bit: u32) -> (Vec<u8>, relon_eval_api::schema_canonical::Schema) {
    use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
    use relon_ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
    use relon_parser::TokenRange;

    let synth_range = TokenRange {
        start: relon_parser::TokenPosition {
            line: 1,
            column: 1,
            offset: 0,
        },
        end: relon_parser::TokenPosition {
            line: 1,
            column: 1,
            offset: 0,
        },
    };
    let t = |op: Op| TaggedOp {
        op,
        range: synth_range,
    };

    let main_schema = Schema {
        name: "MainParams".into(),
        generics: vec![],
        fields: vec![Field {
            name: "x".into(),
            ty: TypeRepr::Int,
            default: None,
        }],
    };
    let return_schema = Schema {
        name: "Ret".into(),
        generics: vec![],
        fields: vec![Field {
            name: "value".into(),
            ty: TypeRepr::Int,
            default: None,
        }],
    };
    let return_layout =
        relon_eval_api::layout::SchemaLayout::offsets_for(&return_schema).expect("return layout");
    let value_offset = return_layout
        .fields
        .iter()
        .find(|f| f.name == "value")
        .map(|f| f.offset as u32)
        .expect("value offset");

    let ir_module = IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".into(),
            params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret: IrType::I32,
            range: synth_range,
            body: vec![
                t(Op::CheckCap { cap_bit }),
                t(Op::ConstI64(42)),
                t(Op::StoreField {
                    offset: value_offset,
                    ty: IrType::I64,
                }),
                t(Op::Return),
            ],
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    };

    let wasm = relon_codegen_wasm::compile_module(&ir_module, &main_schema, &return_schema)
        .expect("compile check-cap module");
    (wasm, main_schema)
}

#[test]
fn with_capabilities_default_denies_cap_check() {
    // Default Capabilities publish a zero-trust bitmap, so the
    // codegen `check_cap` prologue must trap with
    // `WasmCapabilityDenied { cap_bit: 0 }` (`CapabilityBit::ReadsFs`).
    use relon_eval_api::{Capabilities, CapabilityBit};

    let cap_bit = CapabilityBit::ReadsFs.bit_index();
    let (wasm, main_schema) = build_check_cap_module(cap_bit);
    let return_schema = relon_eval_api::schema_canonical::Schema {
        name: "Ret".into(),
        generics: vec![],
        fields: vec![relon_eval_api::schema_canonical::Field {
            name: "value".into(),
            ty: relon_eval_api::schema_canonical::TypeRepr::Int,
            default: None,
        }],
    };
    let aot = WasmAotEvaluator::from_bytes(wasm, main_schema, return_schema)
        .expect("from_bytes")
        .with_capabilities(Capabilities::default());

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(0));

    let err = aot
        .run_main(args)
        .expect_err("zero-trust caps must deny the cap check");
    match err {
        RuntimeError::WasmCapabilityDenied { cap_bit: bit, .. } => {
            assert_eq!(bit, cap_bit, "cap_bit must match the declared check");
        }
        other => panic!("expected WasmCapabilityDenied, got {other:?}"),
    }
}

#[test]
fn with_capabilities_all_granted_allows_cap_check() {
    // `Capabilities::all_granted` lights up every declared bit, so
    // the same module's `check_cap` prologue passes through and the
    // body's constant store reaches the return slot.
    use relon_eval_api::{Capabilities, CapabilityBit};

    let cap_bit = CapabilityBit::ReadsFs.bit_index();
    let (wasm, main_schema) = build_check_cap_module(cap_bit);
    let return_schema = relon_eval_api::schema_canonical::Schema {
        name: "Ret".into(),
        generics: vec![],
        fields: vec![relon_eval_api::schema_canonical::Field {
            name: "value".into(),
            ty: relon_eval_api::schema_canonical::TypeRepr::Int,
            default: None,
        }],
    };
    let aot = WasmAotEvaluator::from_bytes(wasm, main_schema, return_schema)
        .expect("from_bytes")
        .with_capabilities(Capabilities::all_granted());

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(0));

    let out = aot.run_main(args).expect("all_granted caps must allow");
    // The single-field return schema flattens to the inner Int value
    // when read back through `BufferReader`. The 42 here is the
    // `Op::ConstI64(42)` the body stored after the cap-check passed.
    assert_eq!(out, Value::Int(42));
}
