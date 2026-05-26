//! Phase E.2 synthetic recursive `fib` smoke.
//!
//! Constructs a hand-built IR module shaped like the production
//! `fib: (k) => k < 2 ? k : fib(k - 1) + fib(k - 2)` body, but bypasses
//! the AST -> IR lowering pipeline (which still rejects closures as
//! first-class dict values — see Phase F scope). The test proves the
//! LLVM emitter's Phase E.2 multi-function path is wired correctly:
//!
//! * Two functions in one LLVM module (a `fib` helper + a thin entry
//!   that calls `fib(n)`).
//! * `Op::Call { fn_index = stdlib_count + 0, ... }` resolves to the
//!   sibling `FunctionValue` and lowers to a direct LLVM `call`.
//! * Self-recursion (`fib` calls `fib`) works because the helper is
//!   declared before its body is emitted, so the second-pass body
//!   lowering can reference its own `FunctionValue`.
//!
//! When Phase F lifts the IR-level closure-as-value rejection, the
//! production-source fib lowering will produce IR that flows through
//! the same emitter path and this test should keep passing without
//! changes.

use std::collections::HashMap;

use relon_codegen_llvm::LlvmAotEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_ir::stdlib::stdlib_function_count;
use relon_parser::TokenRange;

fn tagged(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

/// Build the synthetic `fib(k)` helper body.
///
/// ```text
/// if k < 2 { k } else { fib(k - 1) + fib(k - 2) }
/// ```
///
/// The helper sits at `ir.funcs[0]`; the `Op::Call` `fn_index` points
/// at `stdlib_function_count() + 0`.
fn build_fib_helper() -> Func {
    let fib_call_idx = stdlib_function_count();

    // `if k < 2 { k } else { fib(k - 1) + fib(k - 2) }`
    let cond_body = vec![
        tagged(Op::LocalGet(0)),
        tagged(Op::ConstI64(2)),
        tagged(Op::Lt(IrType::I64)),
    ];

    // Then arm: push k.
    let then_body = vec![tagged(Op::LocalGet(0))];

    // Else arm: fib(k-1) + fib(k-2).
    let else_body = vec![
        // fib(k - 1)
        tagged(Op::LocalGet(0)),
        tagged(Op::ConstI64(1)),
        tagged(Op::Sub(IrType::I64)),
        tagged(Op::Call {
            fn_index: fib_call_idx,
            arg_count: 1,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
        }),
        // fib(k - 2)
        tagged(Op::LocalGet(0)),
        tagged(Op::ConstI64(2)),
        tagged(Op::Sub(IrType::I64)),
        tagged(Op::Call {
            fn_index: fib_call_idx,
            arg_count: 1,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
        }),
        // sum
        tagged(Op::Add(IrType::I64)),
    ];

    let mut body = cond_body;
    body.push(tagged(Op::If {
        result_ty: IrType::I64,
        then_body,
        else_body,
    }));
    body.push(tagged(Op::Return));

    Func {
        name: "fib".to_string(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    }
}

/// Build the entry function `(n: Int) -> Int : fib(n)`. Sits at
/// `ir.funcs[1]` and is the module's `entry_func_index`.
fn build_entry_calling_fib() -> Func {
    let fib_call_idx = stdlib_function_count();
    let body = vec![
        tagged(Op::LocalGet(0)),
        tagged(Op::Call {
            fn_index: fib_call_idx,
            arg_count: 1,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
        }),
        tagged(Op::Return),
    ];
    Func {
        name: "run_main".to_string(),
        params: vec![IrType::I64],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    }
}

fn build_recursive_fib_module() -> IrModule {
    let fib = build_fib_helper();
    let entry = build_entry_calling_fib();
    IrModule {
        imports: vec![],
        funcs: vec![fib, entry],
        entry_func_index: Some(1),
        closure_table: vec![],
    }
}

#[test]
fn synthetic_recursive_fib_ten() {
    let ir = build_recursive_fib_module();
    let ev = LlvmAotEvaluator::from_ir_direct(ir, vec!["n".to_string()]).expect("compile");

    // Direct legacy fast-path entry — bypasses the HashMap pack.
    let r = ev.run_main_legacy_i64(&[10]).expect("fast path");
    assert_eq!(r, 55, "fib(10) should be 55, got {r}");
}

#[test]
fn synthetic_recursive_fib_through_evaluator_trait() {
    let ir = build_recursive_fib_module();
    let ev = LlvmAotEvaluator::from_ir_direct(ir, vec!["n".to_string()]).expect("compile");
    let mut args = HashMap::new();
    args.insert("n".to_string(), Value::Int(10));
    let result = ev.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(55));
}

#[test]
fn synthetic_recursive_fib_base_cases() {
    let ir = build_recursive_fib_module();
    let ev = LlvmAotEvaluator::from_ir_direct(ir, vec!["n".to_string()]).expect("compile");
    // fib(0) = 0, fib(1) = 1 — both hit the base case directly.
    assert_eq!(ev.run_main_legacy_i64(&[0]).unwrap(), 0);
    assert_eq!(ev.run_main_legacy_i64(&[1]).unwrap(), 1);
    assert_eq!(ev.run_main_legacy_i64(&[2]).unwrap(), 1);
    assert_eq!(ev.run_main_legacy_i64(&[3]).unwrap(), 2);
}

#[test]
fn synthetic_recursive_fib_ir_dump_has_helper() {
    let ir = build_recursive_fib_module();
    let ev = LlvmAotEvaluator::from_ir_direct(ir, vec!["n".to_string()]).expect("compile");
    let dump = ev.emit_ir_dump();
    // The helper's LLVM symbol name follows the `relon_helper_<name>`
    // template the emitter uses. Substring rather than full-text
    // assertion so LLVM's whitespace / metadata is free to drift.
    assert!(
        dump.contains("relon_helper_fib"),
        "IR dump missing the fib helper symbol:\n{dump}"
    );
    assert!(
        dump.contains("relon_llvm_entry"),
        "IR dump missing entry symbol:\n{dump}"
    );
}
