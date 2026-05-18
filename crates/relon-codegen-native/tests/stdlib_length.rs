//! Sandbox-friendly stdlib smoke: exercise the narrow IR subset that
//! a simple stdlib body would lower to — control flow + arithmetic +
//! comparisons over `Int`.
//!
//! v5-beta-1 deliberately defers the IR-side wire-up of `Op::Call`
//! and the variable-length `String` / `List` lowering (those live on
//! `LoadStringPtr` / `ReadStringLen` plus the buffer-relative pointer
//! machinery). This test stands in for HelloWorld scenario #2 from
//! the brief by lowering a hand-written `abs(Int)` body whose op
//! sequence is structurally identical to what `Op::Call` plus the
//! `abs` stdlib body would expand to once inlined.

use std::collections::HashMap;

use relon_codegen_native::{CraneliftAotEvaluator, SandboxConfig};
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

/// Hand-built IR for `abs(arg0) -> Int`:
///
/// ```text
/// if arg0 < 0 { -arg0 } else { arg0 }
/// ```
///
/// The lowering exercises `LocalGet`, `ConstI64`, `Lt`, `If`, `Sub`,
/// `Return` — every op a real `abs(Int)` stdlib body would touch.
fn build_abs_module() -> IrModule {
    let then_body = vec![
        TaggedOp {
            op: Op::ConstI64(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Sub(IrType::I64),
            range: TokenRange::default(),
        },
    ];
    let else_body = vec![TaggedOp {
        op: Op::LocalGet(0),
        range: TokenRange::default(),
    }];
    let body = vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::ConstI64(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Lt(IrType::I64),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::If {
                result_ty: IrType::I64,
                then_body,
                else_body,
            },
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
            params: vec![IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn abs_returns_positive_for_negative_input() {
    let ir = build_abs_module();
    let evaluator =
        CraneliftAotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(-42));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn abs_returns_input_unchanged_for_positive() {
    let ir = build_abs_module();
    let evaluator =
        CraneliftAotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(99));
}

#[test]
fn abs_handles_zero_correctly() {
    let ir = build_abs_module();
    let evaluator =
        CraneliftAotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(0));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(0));
}
