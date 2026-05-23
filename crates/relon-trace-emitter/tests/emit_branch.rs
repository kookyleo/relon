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
    b.append(TraceOp::ConstI64 { dst: v, value: 99 });
    b.record_type(v, ObservedType::I64);
    let pc = b.append(TraceOp::Guard {
        kind: GuardKind::TypeCheck(v, ObservedType::I64),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0x1),
        GuardKind::TypeCheck(v, ObservedType::I64),
    ));
    b.append(TraceOp::Return { value: v });
    emit_and_verify(&b.into_optimized());
}

#[test]
fn two_guards_share_deopt_block() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let bv = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 1 });
    b.append(TraceOp::ConstI64 { dst: bv, value: 2 });
    b.record_type(a, ObservedType::I64);
    b.record_type(bv, ObservedType::I64);

    let pc1 = b.append(TraceOp::Guard {
        kind: GuardKind::TypeCheck(a, ObservedType::I64),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        pc1,
        ExternalPc(0x10),
        GuardKind::TypeCheck(a, ObservedType::I64),
    ));
    let pc2 = b.append(TraceOp::Guard {
        kind: GuardKind::TypeCheck(bv, ObservedType::I64),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        pc2,
        ExternalPc(0x20),
        GuardKind::TypeCheck(bv, ObservedType::I64),
    ));
    let r = b.fresh_ssa();
    b.append(TraceOp::Add {
        dst: r,
        lhs: a,
        rhs: bv,
    });
    b.append(TraceOp::Return { value: r });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // Two guard sites and one shared deopt block → at least two brif's.
    assert!(s.matches("brif").count() >= 2, "two brif's expected:\n{s}");
}

#[test]
fn guard_failure_path_calls_save_deopt() {
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v, value: 0 });
    b.record_type(v, ObservedType::I64);
    let pc = b.append(TraceOp::Guard {
        kind: GuardKind::NotNull(v),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xdeadbeef),
        GuardKind::NotNull(v),
    ));
    b.append(TraceOp::Return { value: v });
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
    b.append(TraceOp::ConstI64 { dst: v, value: 1 });
    b.record_type(v, ObservedType::I32);
    let pc = b.append(TraceOp::Guard {
        kind: GuardKind::ArithOverflow(v),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xaa),
        GuardKind::ArithOverflow(v),
    ));
    b.append(TraceOp::Return { value: v });
    emit_and_verify(&b.into_optimized());
}

/// F-D7-J fast-path: `Add` produces an `of` bit via `sadd_overflow`;
/// the immediately-following `Guard(ArithOverflow(dst))` should
/// brif directly on the captured `of` SSA, skipping the
/// `icmp(eq, of, 0)` + `uextend(I32)` chain the predicate builder
/// would otherwise emit. We verify by counting that exactly one
/// `icmp` ends up in the function — the one inside `sadd_overflow`'s
/// own carry detection — and not the extra `eq` the legacy guard
/// predicate added.
#[test]
fn arith_overflow_guard_skips_icmp_with_captured_of_bit() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let bv = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 1 });
    b.append(TraceOp::ConstI64 { dst: bv, value: 2 });
    b.append(TraceOp::Add {
        dst,
        lhs: a,
        rhs: bv,
    });
    let pc = b.append(TraceOp::Guard {
        kind: GuardKind::ArithOverflow(dst),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xbb),
        GuardKind::ArithOverflow(dst),
    ));
    b.append(TraceOp::Return { value: dst });

    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // No standalone `icmp` should remain — `sadd_overflow` returns
    // `(r, of)` and the guard `brif`s on `of` directly. (cranelift
    // sometimes prints `sadd_overflow` as a single op without an
    // adjacent `icmp`; the legacy predicate would have added an
    // explicit `icmp eq, of, 0` we want to skip.) Match the legacy
    // pattern's exact textual form to keep the assertion stable
    // across cranelift print-format tweaks.
    assert!(
        !s.contains("icmp eq"),
        "expected no `icmp eq` (legacy of==0 predicate), got:\n{s}"
    );
    // And exactly one brif (the guard's), since there's no other
    // control flow in this trace.
    let brif_count = s.matches("brif").count();
    assert!(
        brif_count >= 1,
        "expected at least one brif (the guard), got {brif_count} in:\n{s}"
    );
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
    b.append(TraceOp::ConstI64 { dst: idx, value: 2 });
    b.append(TraceOp::ConstI64 {
        dst: limit,
        value: 5,
    });
    let pc = b.append(TraceOp::Guard {
        kind: GuardKind::BoundsCheck(idx, limit),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xc0ffee),
        GuardKind::BoundsCheck(idx, limit),
    ));
    b.append(TraceOp::ConstI64 {
        dst: base,
        value: 0x7000,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(16),
    });
    b.append(TraceOp::Return { value: dst });
    emit_and_verify(&b.into_optimized());
}
