//! Load / store + bounds-guard emission.
//!
//! Verifies that:
//!   * plain `Load` / `Store` produce valid memory ops.
//!   * `Guard(BoundsCheck,..)` paired with a `Load` lowers to the
//!     conditional-deopt pattern (`brif → deopt_block`).
//!   * `Guard(NotNull,..)` before a `Load` also lowers cleanly.

mod common;

use common::emit_and_verify;
use relon_trace_jit::{ExternalPc, GuardKind, GuardSite, Offset, SsaVar, TraceBuffer, TraceOp};

#[test]
fn load_only_lowers_to_load_inst() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: base,
        value: 0x2000,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(8),
    });
    b.append(TraceOp::Return { value: dst });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(s.contains("load.i64"), "expected load.i64 in:\n{s}");
}

#[test]
fn store_only_lowers_to_store_inst() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let val = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: base,
        value: 0x3000,
    });
    b.append(TraceOp::ConstI64 {
        dst: val,
        value: 0xdead,
    });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: val,
    });
    b.append(TraceOp::Return { value: val });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(s.contains("store"), "expected store in:\n{s}");
}

#[test]
fn bounds_guard_before_load_emits_brif() {
    // The recorder pattern: `Guard(BoundsCheck(idx, limit))` is
    // inserted just before the corresponding `Load`. The emitter
    // should turn the guard into a `brif cond, ok, deopt` pair.
    let mut b = TraceBuffer::new();
    let idx = b.fresh_ssa();
    let limit = b.fresh_ssa();
    let base = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: idx, value: 3 });
    b.append(TraceOp::ConstI64 {
        dst: limit,
        value: 10,
    });
    let guard_pc = b.append(TraceOp::Guard {
        kind: GuardKind::BoundsCheck(idx, limit),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        guard_pc,
        ExternalPc(0xabcd),
        GuardKind::BoundsCheck(idx, limit),
    ));
    b.append(TraceOp::ConstI64 {
        dst: base,
        value: 0x4000,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Return { value: dst });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(s.contains("brif"), "expected brif from guard in:\n{s}");
}

#[test]
fn not_null_guard_lowers_to_brif_pair() {
    let mut b = TraceBuffer::new();
    let p = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: p,
        value: 0x5000,
    });
    let guard_pc = b.append(TraceOp::Guard {
        kind: GuardKind::NotNull(p),
        check: SsaVar::NONE,
    });
    b.record_guard(GuardSite::new(
        guard_pc,
        ExternalPc(0xdeadbeef),
        GuardKind::NotNull(p),
    ));
    b.append(TraceOp::Return { value: p });
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(s.contains("brif"), "expected brif in:\n{s}");
}

#[test]
fn store_followed_by_load_round_trip() {
    // Store then Load from same base / different offsets; verifier
    // should accept the resulting IR even though we don't model the
    // alias relationship.
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let v = b.fresh_ssa();
    let reloaded = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: base,
        value: 0x6000,
    });
    b.append(TraceOp::ConstI64 {
        dst: v,
        value: 0x42,
    });
    b.append(TraceOp::Store {
        base,
        offset: Offset(8),
        src: v,
    });
    b.append(TraceOp::Load {
        dst: reloaded,
        base,
        offset: Offset(8),
    });
    b.append(TraceOp::Return { value: reloaded });
    emit_and_verify(&b.into_optimized());
}
