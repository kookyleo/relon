//! End-to-end emission tests for the arithmetic + compare slice.
//!
//! Each test builds a small `TraceBuffer`, freezes it into an
//! `OptimizedTrace`, and lets the emitter run. The cranelift verifier
//! then certifies the produced IR is well-formed.

mod common;

use common::emit_and_verify;
use relon_trace_jit::{CmpKind, TraceBuffer, TraceOp};

#[test]
fn add_const_constants_round_trip() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 10 });
    b.append(TraceOp::ConstI64 { dst: c, value: 20 });
    b.append(TraceOp::Add {
        dst: r,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::Return { value: r });
    emit_and_verify(&b.into_optimized());
}

#[test]
fn sub_emits_isub_instruction() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 7 });
    b.append(TraceOp::ConstI64 { dst: c, value: 3 });
    b.append(TraceOp::Sub {
        dst: r,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::Return { value: r });
    emit_and_verify(&b.into_optimized());
}

#[test]
fn mul_emits_imul_instruction() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 6 });
    b.append(TraceOp::ConstI64 { dst: c, value: 7 });
    b.append(TraceOp::Mul {
        dst: r,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::Return { value: r });
    emit_and_verify(&b.into_optimized());
}

#[test]
fn div_inserts_divisor_zero_check() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 84 });
    b.append(TraceOp::ConstI64 { dst: c, value: 2 });
    b.append(TraceOp::Div {
        dst: r,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::Return { value: r });
    emit_and_verify(&b.into_optimized());
}

/// F-D8-E.1: smoke-test that `TraceOp::Mod` lowers cleanly through
/// the emitter. Asserts both shape (`srem` + divisor-zero `brif`)
/// and verifier acceptance.
#[test]
fn mod_emits_srem_with_divisor_zero_check() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 47 });
    b.append(TraceOp::ConstI64 { dst: c, value: 10 });
    b.append(TraceOp::Mod {
        dst: r,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::Return { value: r });
    let ctx = emit_and_verify(&b.into_optimized());
    let printed = format!("{}", ctx.func);
    assert!(
        printed.contains("srem"),
        "expected srem instruction, got {printed}"
    );
    assert!(
        printed.contains("brif"),
        "expected divisor-zero brif, got {printed}"
    );
}

#[test]
fn cmp_lt_emits_widened_bool() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 1 });
    b.append(TraceOp::ConstI64 { dst: c, value: 2 });
    b.append(TraceOp::Cmp {
        kind: CmpKind::Lt,
        dst: r,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::Return { value: r });
    let ctx = emit_and_verify(&b.into_optimized());
    let printed = format!("{}", ctx.func);
    assert!(
        printed.contains("icmp slt"),
        "expected icmp slt, got {printed}"
    );
}

#[test]
fn fused_arith_chain_is_well_typed() {
    // (((a + b) - c) * d) — sanity check that intermediate SSA values
    // propagate correctly through the bind/lookup pipeline.
    let mut b = TraceBuffer::new();
    let va = b.fresh_ssa();
    let vb = b.fresh_ssa();
    let vc = b.fresh_ssa();
    let vd = b.fresh_ssa();
    let t1 = b.fresh_ssa();
    let t2 = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: va, value: 5 });
    b.append(TraceOp::ConstI64 { dst: vb, value: 7 });
    b.append(TraceOp::ConstI64 { dst: vc, value: 1 });
    b.append(TraceOp::ConstI64 { dst: vd, value: 4 });
    b.append(TraceOp::Add {
        dst: t1,
        lhs: va,
        rhs: vb,
    });
    b.append(TraceOp::Sub {
        dst: t2,
        lhs: t1,
        rhs: vc,
    });
    b.append(TraceOp::Mul {
        dst: r,
        lhs: t2,
        rhs: vd,
    });
    b.append(TraceOp::Return { value: r });
    emit_and_verify(&b.into_optimized());
}
