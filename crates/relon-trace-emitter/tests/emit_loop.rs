//! Loop marker emission — `MarkLoopHead` → cranelift block,
//! `MarkLoopBack` → unconditional `jump` to the matching header.

mod common;

use common::emit_and_verify;
use cranelift_codegen::Context;
use relon_trace_emitter::{EmitError, TraceEmitter};
use relon_trace_jit::{TraceBuffer, TraceOp};

#[test]
fn simple_loop_lowers_to_jump() {
    let mut b = TraceBuffer::new();
    let acc = b.fresh_ssa();
    b.append(TraceOp::ConstI64(acc, 0));
    b.append(TraceOp::MarkLoopHead { loop_id: 0 });
    let one = b.fresh_ssa();
    b.append(TraceOp::ConstI64(one, 1));
    let next = b.fresh_ssa();
    b.append(TraceOp::Add(next, acc, one));
    b.append(TraceOp::MarkLoopBack { loop_id: 0 });
    b.append(TraceOp::Return(next));
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(s.contains("jump"), "expected jump in:\n{s}");
}

#[test]
fn nested_loops_lower_each_with_its_own_block() {
    let mut b = TraceBuffer::new();
    let outer = b.fresh_ssa();
    b.append(TraceOp::ConstI64(outer, 0));
    b.append(TraceOp::MarkLoopHead { loop_id: 0 });

    let inner = b.fresh_ssa();
    b.append(TraceOp::ConstI64(inner, 0));
    b.append(TraceOp::MarkLoopHead { loop_id: 1 });

    let step = b.fresh_ssa();
    b.append(TraceOp::ConstI64(step, 1));
    let inner_next = b.fresh_ssa();
    b.append(TraceOp::Add(inner_next, inner, step));

    b.append(TraceOp::MarkLoopBack { loop_id: 1 });
    b.append(TraceOp::MarkLoopBack { loop_id: 0 });
    b.append(TraceOp::Return(inner_next));
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // Two MarkLoopHead pairs → at least two `jump` ops.
    assert!(s.matches("jump").count() >= 2, "expected ≥2 jumps in:\n{s}");
}

#[test]
fn unmatched_loop_back_surfaces_error() {
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 0));
    b.append(TraceOp::MarkLoopBack { loop_id: 9 });
    b.append(TraceOp::Return(v));
    let trace = b.into_optimized();
    let mut ctx = Context::new();
    let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
    assert!(matches!(err, EmitError::UnmatchedLoopBack(9)));
}

#[test]
fn loop_body_with_no_back_edge_still_verifies() {
    // The head's forward edge is wired; if no back edge ever closes
    // it, cranelift would normally complain about an unsealed block —
    // but we seal it in the dummy-block stub the Return emission
    // creates. The verifier should accept the resulting linear flow.
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 0));
    b.append(TraceOp::MarkLoopHead { loop_id: 0 });
    b.append(TraceOp::MarkLoopBack { loop_id: 0 });
    b.append(TraceOp::Return(v));
    emit_and_verify(&b.into_optimized());
}
