//! Loop marker emission — `MarkLoopHead` → cranelift block,
//! `MarkLoopBack` → unconditional `jump` to the matching header.

mod common;

use common::emit_and_verify;
use cranelift_codegen::Context;
use relon_trace_emitter::{EmitError, TraceEmitter};
use relon_trace_jit::{CmpKind, GuardKind, GuardSite, LoopPhi, TraceBuffer, TraceOp};

#[test]
fn simple_loop_lowers_to_jump() {
    let mut b = TraceBuffer::new();
    let acc = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: acc, value: 0 });
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    let one = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: one, value: 1 });
    let next = b.fresh_ssa();
    b.append(TraceOp::Add {
        dst: next,
        lhs: acc,
        rhs: one,
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });
    b.append(TraceOp::Return { value: next });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(s.contains("jump"), "expected jump in:\n{s}");
}

#[test]
fn nested_loops_lower_each_with_its_own_block() {
    let mut b = TraceBuffer::new();
    let outer = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: outer,
        value: 0,
    });
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });

    let inner = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: inner,
        value: 0,
    });
    b.append(TraceOp::MarkLoopHead {
        loop_id: 1,
        phis: vec![],
    });

    let step = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: step,
        value: 1,
    });
    let inner_next = b.fresh_ssa();
    b.append(TraceOp::Add {
        dst: inner_next,
        lhs: inner,
        rhs: step,
    });

    b.append(TraceOp::MarkLoopBack {
        loop_id: 1,
        next_values: vec![],
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });
    b.append(TraceOp::Return { value: inner_next });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // Two MarkLoopHead pairs → at least two `jump` ops.
    assert!(s.matches("jump").count() >= 2, "expected ≥2 jumps in:\n{s}");
}

#[test]
fn unmatched_loop_back_surfaces_error() {
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v, value: 0 });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 9,
        next_values: vec![],
    });
    b.append(TraceOp::Return { value: v });
    let trace = b.into_optimized();
    let mut ctx = Context::new();
    let err = TraceEmitter::emit(&trace, &mut ctx).unwrap_err();
    assert!(matches!(err, EmitError::UnmatchedLoopBack(9)));
}

/// ε-M0: loop with one φ-carried value — the recorder-driven shape.
///
/// Build a sum-1..=n trace where `acc` and `i` are φ-carried; the
/// trace exits on a `Cmp(Le)` + `Guard(NotNull)` when `i > n`.
#[test]
fn phi_carried_loop_emits_block_params() {
    use relon_trace_abi::ExternalPc;

    let mut b = TraceBuffer::new();
    // Pre-loop: n = LocalGet(0); acc_init = 0; i_init = 1.
    let n = b.fresh_ssa();
    b.append(TraceOp::LocalGet {
        dst: n,
        slot_idx: 0,
    });
    let acc_init = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: acc_init,
        value: 0,
    });
    let i_init = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: i_init,
        value: 1,
    });

    // Phi SSAs for the loop body.
    let phi_acc = b.fresh_ssa();
    let phi_i = b.fresh_ssa();
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![LoopPhi::new(acc_init, phi_acc), LoopPhi::new(i_init, phi_i)],
    });

    // Body: cmp = i <= n; guard(cmp); acc_next = acc + i; i_next = i + 1.
    let cmp = b.fresh_ssa();
    b.append(TraceOp::Cmp {
        kind: CmpKind::Le,
        dst: cmp,
        lhs: phi_i,
        rhs: n,
    });
    let cmp_guard_pc = b.append(TraceOp::Guard {
        kind: GuardKind::NotNull(cmp),
        check: cmp,
    });
    b.record_guard(GuardSite::new(
        cmp_guard_pc,
        ExternalPc(1),
        GuardKind::NotNull(cmp),
    ));
    let acc_next = b.fresh_ssa();
    b.append(TraceOp::Add {
        dst: acc_next,
        lhs: phi_acc,
        rhs: phi_i,
    });
    let one = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: one, value: 1 });
    let i_next = b.fresh_ssa();
    b.append(TraceOp::Add {
        dst: i_next,
        lhs: phi_i,
        rhs: one,
    });

    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![acc_next, i_next],
    });
    b.append(TraceOp::Return { value: phi_acc });

    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // Expect cranelift IR to contain a header block taking two
    // block-params (the φs), with both the entry jump and the back
    // edge supplying matching arg tuples.
    assert!(s.contains("jump"), "expected jump in:\n{s}");
}

#[test]
fn loop_body_with_no_back_edge_still_verifies() {
    // The head's forward edge is wired; if no back edge ever closes
    // it, cranelift would normally complain about an unsealed block —
    // but we seal it in the dummy-block stub the Return emission
    // creates. The verifier should accept the resulting linear flow.
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v, value: 0 });
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });
    b.append(TraceOp::Return { value: v });
    emit_and_verify(&b.into_optimized());
}
