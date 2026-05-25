//! End-to-end smoke: `#main(Int x, Int y) -> Int : x + y` lowered
//! through synthetic IR + cranelift codegen + JIT, then invoked via
//! `Evaluator::run_main`.
//!
//! Covers the v5-beta-1 HelloWorld scenario #1 from the brief. We
//! build the IR directly because the production parse + analyze +
//! lower pipeline emits buffer-protocol IR (out_ptr-relative writes,
//! schema layout, ...) that the cranelift backend doesn't yet speak.
//! v5-beta-2 wires `from_source` through the full lowering pipeline.

use std::collections::HashMap;

use relon_codegen_native::{AotEvaluator, SandboxConfig};
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

fn build_arith_ir(op: Op) -> IrModule {
    let body = vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::LocalGet(1),
            range: TokenRange::default(),
        },
        TaggedOp {
            op,
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ];
    let func = Func {
        name: "run_main".to_string(),
        params: vec![IrType::I64, IrType::I64],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    };
    IrModule {
        imports: vec![],
        funcs: vec![func],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

fn build_evaluator(ir: IrModule) -> AotEvaluator {
    AotEvaluator::from_ir_direct(
        ir,
        SandboxConfig::default(),
        vec!["x".to_string(), "y".to_string()],
    )
    .expect("compile")
}

#[test]
fn add_two_ints_returns_expected_sum() {
    let ir = build_arith_ir(Op::Add(IrType::I64));
    let evaluator = build_evaluator(ir);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn multiplication_runs_through_the_arithmetic_path() {
    let ir = build_arith_ir(Op::Mul(IrType::I64));
    let evaluator = build_evaluator(ir);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));
    args.insert("y".to_string(), Value::Int(6));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn subtraction_handles_negative_results() {
    let ir = build_arith_ir(Op::Sub(IrType::I64));
    let evaluator = build_evaluator(ir);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(3));
    args.insert("y".to_string(), Value::Int(10));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(-7));
}

#[test]
fn modulo_returns_remainder() {
    let ir = build_arith_ir(Op::Mod(IrType::I64));
    let evaluator = build_evaluator(ir);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(17));
    args.insert("y".to_string(), Value::Int(5));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(2));
}

#[test]
fn type_mismatch_arg_surfaces_typed_error() {
    let ir = build_arith_ir(Op::Add(IrType::I64));
    let evaluator = build_evaluator(ir);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(1));
    args.insert("y".to_string(), Value::String("not an int".into()));
    let err = evaluator.run_main(args).expect_err("type mismatch");
    let msg = format!("{err}");
    assert!(
        msg.contains("type mismatch") || msg.contains("Int"),
        "{msg}"
    );
}

#[test]
fn cold_start_completes_in_under_50_ms() {
    // Smoke check: cranelift JIT compile of a 4-op function must
    // finish quickly. The wasm-AOT cold start runs ~2 ms; cranelift
    // should be comparable or faster for this minimal shape.
    let start = std::time::Instant::now();
    let ir = build_arith_ir(Op::Add(IrType::I64));
    let _evaluator = build_evaluator(ir);
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(50),
        "cranelift cold start regression: {:?}",
        elapsed
    );
}
