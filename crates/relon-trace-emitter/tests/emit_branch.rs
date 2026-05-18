//! Guard-driven branch emission (BranchTaken-style guards).
//!
//! These traces test the emitter's "guard predicate → brif → ok /
//! deopt" pattern with multiple guards sharing the same deopt block.

mod common;

use common::emit_and_verify;
use relon_trace_jit::{
    ExternalPc, GuardKind, GuardSite, ObservedType, Offset, SsaVar, TraceBuffer, TraceOp,
};

#[test]
fn single_type_check_guard_lowers() {
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 99));
    b.record_type(v, ObservedType::I64);
    let pc = b.append(TraceOp::Guard(
        GuardKind::TypeCheck(v, ObservedType::I64),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0x1),
        GuardKind::TypeCheck(v, ObservedType::I64),
    ));
    b.append(TraceOp::Return(v));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn two_guards_share_deopt_block() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let bv = b.fresh_ssa();
    b.append(TraceOp::ConstI64(a, 1));
    b.append(TraceOp::ConstI64(bv, 2));
    b.record_type(a, ObservedType::I64);
    b.record_type(bv, ObservedType::I64);

    let pc1 = b.append(TraceOp::Guard(
        GuardKind::TypeCheck(a, ObservedType::I64),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc1,
        ExternalPc(0x10),
        GuardKind::TypeCheck(a, ObservedType::I64),
    ));
    let pc2 = b.append(TraceOp::Guard(
        GuardKind::TypeCheck(bv, ObservedType::I64),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc2,
        ExternalPc(0x20),
        GuardKind::TypeCheck(bv, ObservedType::I64),
    ));
    let r = b.fresh_ssa();
    b.append(TraceOp::Add(r, a, bv));
    b.append(TraceOp::Return(r));
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // Two guard sites and one shared deopt block → at least two brif's.
    assert!(s.matches("brif").count() >= 2, "two brif's expected:\n{s}");
}

#[test]
fn guard_failure_path_calls_save_deopt() {
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 0));
    b.record_type(v, ObservedType::I64);
    let pc = b.append(TraceOp::Guard(GuardKind::NotNull(v), SsaVar::NONE));
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xdeadbeef),
        GuardKind::NotNull(v),
    ));
    b.append(TraceOp::Return(v));
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // The deopt block always emits a call into the host hook. The
    // hook is imported as `u0:0` (User namespace 0, index 0 = SaveDeopt).
    assert!(
        s.contains("call fn0") || s.contains("u0:0"),
        "expected save_deopt call in:\n{s}"
    );
}

#[test]
fn arith_overflow_guard_lowers_predicate() {
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 1));
    b.record_type(v, ObservedType::I32);
    let pc = b.append(TraceOp::Guard(GuardKind::ArithOverflow(v), SsaVar::NONE));
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xaa),
        GuardKind::ArithOverflow(v),
    ));
    b.append(TraceOp::Return(v));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn guard_then_load_chain_verifies() {
    // Guard(BoundsCheck) ↦ Load: the guard's ok path is the load's
    // entry. Verifier should accept the resulting linear-block layout.
    let mut b = TraceBuffer::new();
    let idx = b.fresh_ssa();
    let limit = b.fresh_ssa();
    let base = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(idx, 2));
    b.append(TraceOp::ConstI64(limit, 5));
    let pc = b.append(TraceOp::Guard(
        GuardKind::BoundsCheck(idx, limit),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xc0ffee),
        GuardKind::BoundsCheck(idx, limit),
    ));
    b.append(TraceOp::ConstI64(base, 0x7000));
    b.append(TraceOp::Load(dst, base, Offset(16)));
    b.append(TraceOp::Return(dst));
    emit_and_verify(&b.into_optimized());
}
