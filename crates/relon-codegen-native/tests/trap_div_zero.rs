//! Sandbox guarantee #2 verification: divide-by-zero must trap
//! before producing a result, and the trap must lift into the
//! standard `RuntimeError::DivisionByZero` variant.

use std::collections::HashMap;

use relon_codegen_native::{AotEvaluator, SandboxConfig};
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

fn build_div_module(div_op: Op) -> IrModule {
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
            op: div_op,
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ];
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn integer_div_by_zero_traps_with_division_error() {
    let ir = build_div_module(Op::Div(IrType::I64));
    let evaluator = AotEvaluator::from_ir_direct(
        ir,
        SandboxConfig::default(),
        vec!["x".to_string(), "y".to_string()],
    )
    .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(10));
    args.insert("y".to_string(), Value::Int(0));
    let err = evaluator.run_main(args).expect_err("must trap");
    assert!(
        matches!(err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero, got {err:?}"
    );
}

#[test]
fn modulo_by_zero_traps_with_division_error() {
    let ir = build_div_module(Op::Mod(IrType::I64));
    let evaluator = AotEvaluator::from_ir_direct(
        ir,
        SandboxConfig::default(),
        vec!["x".to_string(), "y".to_string()],
    )
    .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(10));
    args.insert("y".to_string(), Value::Int(0));
    let err = evaluator.run_main(args).expect_err("must trap");
    assert!(
        matches!(err, RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero, got {err:?}"
    );
}

#[test]
fn divide_with_non_zero_divisor_succeeds() {
    // Sanity: the guard must not fire on the normal path.
    let ir = build_div_module(Op::Div(IrType::I64));
    let evaluator = AotEvaluator::from_ir_direct(
        ir,
        SandboxConfig::default(),
        vec!["x".to_string(), "y".to_string()],
    )
    .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(20));
    args.insert("y".to_string(), Value::Int(4));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(5));
}
