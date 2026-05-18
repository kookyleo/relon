//! External `Call` lowering — resolution through the host hook then
//! `call_indirect` on the returned pointer.

mod common;

use common::emit_and_verify;
use relon_trace_jit::{EffectClass, FuncId, TraceBuffer, TraceOp};

#[test]
fn call_no_args_pure_lowers() {
    let mut b = TraceBuffer::new();
    let r = b.fresh_ssa();
    b.append(TraceOp::Call(r, FuncId(0), vec![], EffectClass::Pure));
    b.append(TraceOp::Return(r));
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
    b.append(TraceOp::ConstI64(a, 1));
    b.append(TraceOp::ConstI64(c, 2));
    b.append(TraceOp::Call(
        r,
        FuncId(42),
        vec![a, c],
        EffectClass::ReadOnly,
    ));
    b.append(TraceOp::Return(r));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn call_recoverable_write_effect_is_emitted() {
    // RecoverableWrite shouldn't reject. The runtime path will
    // capture the before-image; the emitter just lowers the call.
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64(a, 0xdead));
    b.append(TraceOp::Call(
        r,
        FuncId(7),
        vec![a],
        EffectClass::RecoverableWrite,
    ));
    b.append(TraceOp::Return(r));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn unrecoverable_call_surfaces_emit_error() {
    use cranelift_codegen::Context;
    use relon_trace_emitter::{EmitError, TraceEmitter};
    let mut b = TraceBuffer::new();
    let r = b.fresh_ssa();
    b.append(TraceOp::Call(
        r,
        FuncId(99),
        vec![],
        EffectClass::Unrecoverable,
    ));
    b.append(TraceOp::Return(r));
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
    b.append(TraceOp::ConstI64(a, 5));
    b.append(TraceOp::Call(r1, FuncId(1), vec![a], EffectClass::Pure));
    b.append(TraceOp::Call(r2, FuncId(2), vec![r1], EffectClass::Pure));
    b.append(TraceOp::Return(r2));
    emit_and_verify(&b.into_optimized());
}
