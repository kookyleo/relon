//! External `Call` lowering — resolution through the host hook then
//! `call_indirect` on the returned pointer.

mod common;

use common::emit_and_verify;
use relon_trace_jit::{EffectClass, FuncId, TraceBuffer, TraceOp};

#[test]
fn call_no_args_pure_lowers() {
    let mut b = TraceBuffer::new();
    let r = b.fresh_ssa();
    b.append(TraceOp::Call {
        dst: r,
        func: FuncId(0),
        args: vec![],
        effect: EffectClass::Pure,
    });
    b.append(TraceOp::Return { value: r });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(
        s.contains("call_indirect"),
        "expected call_indirect in:\n{s}"
    );
}

#[test]
fn call_with_two_args_lowers() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 1 });
    b.append(TraceOp::ConstI64 { dst: c, value: 2 });
    b.append(TraceOp::Call {
        dst: r,
        func: FuncId(42),
        args: vec![a, c],
        effect: EffectClass::ReadOnly,
    });
    b.append(TraceOp::Return { value: r });
    emit_and_verify(&b.into_optimized());
}

#[test]
fn call_recoverable_write_effect_is_emitted() {
    // RecoverableWrite shouldn't reject. The runtime path will
    // capture the before-image; the emitter just lowers the call.
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: a,
        value: 0xdead,
    });
    b.append(TraceOp::Call {
        dst: r,
        func: FuncId(7),
        args: vec![a],
        effect: EffectClass::RecoverableWrite,
    });
    b.append(TraceOp::Return { value: r });
    emit_and_verify(&b.into_optimized());
}

#[test]
fn unrecoverable_call_surfaces_emit_error() {
    use cranelift_codegen::Context;
    use relon_trace_emitter::{EmitError, TraceEmitter};
    let mut b = TraceBuffer::new();
    let r = b.fresh_ssa();
    b.append(TraceOp::Call {
        dst: r,
        func: FuncId(99),
        args: vec![],
        effect: EffectClass::Unrecoverable,
    });
    b.append(TraceOp::Return { value: r });
    let trace = b.into_optimized();
    let mut ctx = Context::new();
    let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
    assert!(matches!(err, EmitError::UnrecoverableEffectInTrace));
}

#[test]
fn chained_calls_propagate_through_ssa() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let r1 = b.fresh_ssa();
    let r2 = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 5 });
    b.append(TraceOp::Call {
        dst: r1,
        func: FuncId(1),
        args: vec![a],
        effect: EffectClass::Pure,
    });
    b.append(TraceOp::Call {
        dst: r2,
        func: FuncId(2),
        args: vec![r1],
        effect: EffectClass::Pure,
    });
    b.append(TraceOp::Return { value: r2 });
    emit_and_verify(&b.into_optimized());
}
