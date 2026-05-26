//! Stage 5 Phase C.1: full `Op::CallNative` indirect dispatch
//! through the capability vtable. Each test registers a host fn at
//! a known cap-bit slot, builds IR that issues a `CallNative`, and
//! asserts that the JIT runs the dispatch correctly (or traps on the
//! capability-denied path).
//!
//! Host fn ABI: every host fn is exposed as `extern "C"` and takes
//! its declared `param_tys` directly (no marshaling) — pointer-
//! indirect args flow through as arena-relative i32 offsets. The
//! cranelift call-site builds the SigRef from the IR's
//! `param_tys + ret_ty` per call, so each `#native` import can
//! carry a different signature.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use relon_codegen_native::{AotEvaluator, CapabilityVtable, HostFnPtr, SandboxConfig};
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, NativeImport, Op, TaggedOp};
use relon_parser::TokenRange;

fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

/// Host fn `nullary_42() -> i64` returns a fixed sentinel so tests
/// can assert the dispatch landed on the right slot.
unsafe extern "C" fn nullary_42() -> i64 {
    42
}

/// Host fn `add_one(x: i64) -> i64` exercises a single-arg path.
unsafe extern "C" fn add_one(x: i64) -> i64 {
    x + 1
}

/// Host fn `add_two(a: i64, b: i64) -> i64` exercises the two-arg path.
unsafe extern "C" fn add_two(a: i64, b: i64) -> i64 {
    a + b
}

/// Host fn that mutates a shared global state. Tests the visible
/// side-effect path so we know the call actually ran.
static SIDE_EFFECT: AtomicI64 = AtomicI64::new(0);
unsafe extern "C" fn record_side_effect(x: i64) -> i64 {
    SIDE_EFFECT.store(x, Ordering::SeqCst);
    x * 2
}

fn legacy_module(imports: Vec<NativeImport>, body: Vec<TaggedOp>, params: Vec<IrType>) -> IrModule {
    IrModule {
        imports,
        funcs: vec![Func {
            name: "run_main".to_string(),
            params,
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn call_native_nullary_returns_host_fn_result() {
    // IR: `return nullary_42()`. The import is at idx 0, cap_bit 0.
    let imports = vec![NativeImport {
        name: "nullary_42".into(),
        param_tys: vec![],
        ret_ty: IrType::I64,
        cap_bit: 0,
    }];
    let body = vec![
        t(Op::CallNative {
            import_idx: 0,
            param_tys: vec![],
            ret_ty: IrType::I64,
            cap_bit: 0,
        }),
        t(Op::Return),
    ];
    let ir = legacy_module(imports, body, vec![]);
    let mut evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec![]).expect("compile");
    let mut vt = CapabilityVtable::with_capacity(64);
    // SAFETY: the lifetime-erased cast matches the actual host fn
    // signature; the IR-declared shape matches the host fn shape.
    let ptr: HostFnPtr = unsafe { std::mem::transmute(nullary_42 as *const ()) };
    vt.register(0, ptr);
    evaluator.install_capabilities_mut(Arc::new(vt));

    let result = evaluator.run_main(HashMap::new()).expect("run_main");
    assert_eq!(result, Value::Int(42));
}

#[test]
fn call_native_with_one_arg_passes_arg_through_to_host() {
    let imports = vec![NativeImport {
        name: "add_one".into(),
        param_tys: vec![IrType::I64],
        ret_ty: IrType::I64,
        cap_bit: 0,
    }];
    let body = vec![
        t(Op::LocalGet(0)),
        t(Op::CallNative {
            import_idx: 0,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: 0,
        }),
        t(Op::Return),
    ];
    let ir = legacy_module(imports, body, vec![IrType::I64]);
    let mut evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");
    let mut vt = CapabilityVtable::with_capacity(64);
    let ptr: HostFnPtr = unsafe { std::mem::transmute(add_one as *const ()) };
    vt.register(0, ptr);
    evaluator.install_capabilities_mut(Arc::new(vt));

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(100));
}

#[test]
fn call_native_with_two_args_passes_both_args_in_declaration_order() {
    let imports = vec![NativeImport {
        name: "add_two".into(),
        param_tys: vec![IrType::I64, IrType::I64],
        ret_ty: IrType::I64,
        cap_bit: 0,
    }];
    let body = vec![
        t(Op::LocalGet(0)),
        t(Op::LocalGet(1)),
        t(Op::CallNative {
            import_idx: 0,
            param_tys: vec![IrType::I64, IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: 0,
        }),
        t(Op::Return),
    ];
    let ir = legacy_module(imports, body, vec![IrType::I64, IrType::I64]);
    let mut evaluator = AotEvaluator::from_ir_direct(
        ir,
        SandboxConfig::default(),
        vec!["a".to_string(), "b".to_string()],
    )
    .expect("compile");
    let mut vt = CapabilityVtable::with_capacity(64);
    let ptr: HostFnPtr = unsafe { std::mem::transmute(add_two as *const ()) };
    vt.register(0, ptr);
    evaluator.install_capabilities_mut(Arc::new(vt));

    let mut args = HashMap::new();
    args.insert("a".to_string(), Value::Int(7));
    args.insert("b".to_string(), Value::Int(11));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(18));
}

#[test]
fn call_native_traps_when_capability_slot_is_empty() {
    let imports = vec![NativeImport {
        name: "add_one".into(),
        param_tys: vec![IrType::I64],
        ret_ty: IrType::I64,
        cap_bit: 5,
    }];
    let body = vec![
        t(Op::LocalGet(0)),
        t(Op::CallNative {
            import_idx: 0,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: 5,
        }),
        t(Op::Return),
    ];
    let ir = legacy_module(imports, body, vec![IrType::I64]);
    let evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");
    // No vtable registration: slot 5 stays empty.

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(1));
    let err = evaluator.run_main(args).expect_err("must trap");
    assert!(
        matches!(err, RuntimeError::WasmCapabilityDenied { .. }),
        "expected WasmCapabilityDenied, got {err:?}"
    );
}

#[test]
fn call_native_mutates_host_side_state_via_side_effect() {
    let imports = vec![NativeImport {
        name: "record_side_effect".into(),
        param_tys: vec![IrType::I64],
        ret_ty: IrType::I64,
        cap_bit: 0,
    }];
    let body = vec![
        t(Op::LocalGet(0)),
        t(Op::CallNative {
            import_idx: 0,
            param_tys: vec![IrType::I64],
            ret_ty: IrType::I64,
            cap_bit: 0,
        }),
        t(Op::Return),
    ];
    let ir = legacy_module(imports, body, vec![IrType::I64]);
    let mut evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");
    let mut vt = CapabilityVtable::with_capacity(64);
    let ptr: HostFnPtr = unsafe { std::mem::transmute(record_side_effect as *const ()) };
    vt.register(0, ptr);
    evaluator.install_capabilities_mut(Arc::new(vt));

    SIDE_EFFECT.store(0, Ordering::SeqCst);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(777));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(1554)); // 777 * 2
    assert_eq!(SIDE_EFFECT.load(Ordering::SeqCst), 777);
}

#[test]
fn call_native_with_param_shape_mismatch_surfaces_codegen_error() {
    // Declared import expects (I64), IR's CallNative says (I64, I64).
    // Codegen rejects the mismatch up-front rather than miscompiling.
    let imports = vec![NativeImport {
        name: "add_one".into(),
        param_tys: vec![IrType::I64],
        ret_ty: IrType::I64,
        cap_bit: 0,
    }];
    let body = vec![
        t(Op::LocalGet(0)),
        t(Op::LocalGet(0)),
        t(Op::CallNative {
            import_idx: 0,
            param_tys: vec![IrType::I64, IrType::I64], // diverges from import
            ret_ty: IrType::I64,
            cap_bit: 0,
        }),
        t(Op::Return),
    ];
    let ir = legacy_module(imports, body, vec![IrType::I64]);
    let result = AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()]);
    assert!(result.is_err(), "expected Codegen mismatch");
}
