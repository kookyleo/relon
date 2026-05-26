//! Phase A bootstrap smoke: hand-built IR → LLVM emit → JIT
//! invoke → typed return. Mirrors the cranelift crate's
//! `helloworld_arith.rs` shape so a side-by-side comparison of the
//! two AOT backends shares the same input fixtures.
//!
//! Each test exercises one supported `Op::*` (Add / Sub / Mul) plus
//! the `LocalGet` + `Return` skeleton. `ConstI64` is exercised by
//! the dedicated `x_plus_one_constant_fold` test that mirrors the
//! Phase A user-facing bootstrap example `#main(Int x) -> Int : x + 1`.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

fn tagged(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

fn build_binop_ir(op: Op) -> IrModule {
    let body = vec![
        tagged(Op::LocalGet(0)),
        tagged(Op::LocalGet(1)),
        tagged(op),
        tagged(Op::Return),
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

fn build_evaluator(ir: IrModule, param_names: Vec<&str>) -> LlvmAotEvaluator {
    LlvmAotEvaluator::from_ir_direct(ir, param_names.into_iter().map(String::from).collect())
        .expect("compile")
}

#[test]
fn add_two_ints_returns_expected_sum() {
    let ir = build_binop_ir(Op::Add(IrType::I64));
    let evaluator = build_evaluator(ir, vec!["x", "y"]);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn subtraction_handles_negative_results() {
    let ir = build_binop_ir(Op::Sub(IrType::I64));
    let evaluator = build_evaluator(ir, vec!["x", "y"]);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(3));
    args.insert("y".to_string(), Value::Int(10));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(-7));
}

#[test]
fn multiplication_runs_through_the_arithmetic_path() {
    let ir = build_binop_ir(Op::Mul(IrType::I64));
    let evaluator = build_evaluator(ir, vec!["x", "y"]);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));
    args.insert("y".to_string(), Value::Int(6));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

/// `#main(Int x) -> Int : x + 1` — the canonical Phase A bootstrap
/// program. Exercises `LocalGet` + `ConstI64` + `Add` end-to-end on
/// a single-arg signature so the trampoline arity-1 branch is hot.
#[test]
fn x_plus_one_constant_fold() {
    let body = vec![
        tagged(Op::LocalGet(0)),
        tagged(Op::ConstI64(1)),
        tagged(Op::Add(IrType::I64)),
        tagged(Op::Return),
    ];
    let func = Func {
        name: "run_main".to_string(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    };
    let ir = IrModule {
        imports: vec![],
        funcs: vec![func],
        entry_func_index: Some(0),
        closure_table: vec![],
    };
    let ev = build_evaluator(ir, vec!["x"]);

    // Direct fast-path entry — bypasses the HashMap pack.
    let r = ev.run_main_legacy_i64(&[41]).expect("fast path");
    assert_eq!(r, 42);

    // HashMap entry — exercises the Evaluator-trait surface.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(41));
    assert_eq!(ev.run_main(args).expect("evaluator path"), Value::Int(42));

    // The dumped IR should mention the entry symbol and at least one
    // `add` instruction. We don't pin the full text because LLVM may
    // adjust whitespace / metadata between versions; the substring
    // checks catch a regression that dropped the function entirely.
    let dump = ev.emit_ir_dump();
    assert!(
        dump.contains("relon_llvm_entry"),
        "dumped IR missing entry symbol:\n{dump}"
    );
    assert!(
        dump.contains("add") || dump.contains("Add"),
        "dumped IR missing add instruction:\n{dump}"
    );
}

/// `#main() -> Int : 42` — exercises the arity-0 trampoline branch
/// and the `ConstI64` literal push.
#[test]
fn arity_zero_returns_constant() {
    let body = vec![tagged(Op::ConstI64(42)), tagged(Op::Return)];
    let func = Func {
        name: "run_main".to_string(),
        params: vec![],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    };
    let ir = IrModule {
        imports: vec![],
        funcs: vec![func],
        entry_func_index: Some(0),
        closure_table: vec![],
    };
    let ev = build_evaluator(ir, vec![]);
    let r = ev.run_main_legacy_i64(&[]).expect("arity-0 call");
    assert_eq!(r, 42);
}
