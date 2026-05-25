//! Sandbox guarantee #3: capability gating.
//!
//! Verifies that a `CheckCap { cap_bit }` op:
//!
//! * succeeds (no trap) when the host has registered a host fn at
//!   that cap_bit;
//! * traps with `WasmCapabilityDenied` when the slot is empty.
//!
//! Covers HelloWorld scenario #3 from the brief.

use std::collections::HashMap;
use std::sync::Arc;

use relon_codegen_native::{CapabilityVtable, AotEvaluator, HostFnPtr, SandboxConfig};
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;

/// Stand-in for a host-registered `#native fn now() -> Int with
/// caps[time]`. Receives the IR-call's i64 arg (unused for nullary
/// host fns) and returns a fixed value so tests can assert against
/// the visible side effect.
unsafe extern "C" fn now_stub(_arg: i64) -> i64 {
    1_700_000_000
}

/// Build IR that runs `CheckCap { cap_bit: 7 }` then returns the
/// first argument unchanged. If the gate fires the body is never
/// reached.
fn build_gated_module(cap_bit: u32) -> IrModule {
    IrModule {
        imports: vec![],
        funcs: vec![Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64],
            ret: IrType::I64,
            body: vec![
                TaggedOp {
                    op: Op::CheckCap { cap_bit },
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::LocalGet(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            range: TokenRange::default(),
        }],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

#[test]
fn check_cap_traps_when_capability_unregistered() {
    let ir = build_gated_module(7);
    let evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let err = evaluator.run_main(args).expect_err("must trap");
    assert!(
        matches!(err, RuntimeError::WasmCapabilityDenied { .. }),
        "expected WasmCapabilityDenied, got {err:?}"
    );
}

#[test]
fn check_cap_succeeds_when_host_registers_fn_at_cap_bit() {
    let ir = build_gated_module(7);
    let mut evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");

    // Register the host fn at the matching cap_bit and re-install
    // the vtable on the evaluator.
    let mut vt = CapabilityVtable::with_capacity(64);
    let fn_ptr: HostFnPtr = now_stub;
    vt.register(7, fn_ptr);
    evaluator.install_capabilities_mut(Arc::new(vt));

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(99));
}

#[test]
fn no_cap_bit_marker_elides_the_gate() {
    // `relon_ir::ir::NO_CAPABILITY_BIT` means "no capability
    // required"; the codegen pass must skip the gate entirely on
    // that path.
    let ir = build_gated_module(relon_ir::ir::NO_CAPABILITY_BIT);
    let evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let result = evaluator.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(99));
}

#[test]
fn unrelated_cap_bit_does_not_satisfy_a_different_gate() {
    let ir = build_gated_module(7);
    let mut evaluator =
        AotEvaluator::from_ir_direct(ir, SandboxConfig::default(), vec!["x".to_string()])
            .expect("compile");

    // Register the host fn at a *different* cap_bit; the gate must
    // still trap.
    let mut vt = CapabilityVtable::with_capacity(64);
    let fn_ptr: HostFnPtr = now_stub;
    vt.register(3, fn_ptr);
    evaluator.install_capabilities_mut(Arc::new(vt));

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(99));
    let err = evaluator.run_main(args).expect_err("must trap");
    assert!(matches!(err, RuntimeError::WasmCapabilityDenied { .. }));
}
