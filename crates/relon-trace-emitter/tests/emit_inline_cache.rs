//! Inline-cache fast-path emission.
//!
//! The pattern these tests assert lives on the boundary between the
//! emitter and the v6-γ phase: a type-spec guard ahead of an external
//! Call models the "type-stable IC hit" path; a type-mismatch causes
//! the guard to fall through to the deopt block. The emitter today
//! generates the guard + call sequence verbatim; the IC table lookup
//! is the host hook `__relon_trace_inline_cache_lookup`, declared by
//! the emitter so the v6-γ integration phase can wire it without
//! patching the emitter.

mod common;

use common::emit_and_verify;
use relon_trace_jit::{
    EffectClass, ExternalPc, FuncId, GuardKind, GuardSite, ObservedType, SsaVar, TraceBuffer,
    TraceOp,
};

#[test]
fn type_spec_guard_before_call_lowers() {
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 100));
    b.record_type(v, ObservedType::I64);
    let pc = b.append(TraceOp::Guard(
        GuardKind::TypeCheck(v, ObservedType::I64),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xfeed),
        GuardKind::TypeCheck(v, ObservedType::I64),
    ));
    let r = b.fresh_ssa();
    b.append(TraceOp::Call(r, FuncId(11), vec![v], EffectClass::Pure));
    b.append(TraceOp::Return(r));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn type_mismatch_lowers_to_const_zero_predicate() {
    // Observed type is I64 but the guard demands Bool — the emitter
    // currently emits `iconst.i32 0`, sending the guard straight to
    // the deopt block. Verifier should accept the structurally valid
    // function regardless.
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 1));
    b.record_type(v, ObservedType::I64);
    let pc = b.append(TraceOp::Guard(
        GuardKind::TypeCheck(v, ObservedType::Bool),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc,
        ExternalPc(0xcaca),
        GuardKind::TypeCheck(v, ObservedType::Bool),
    ));
    b.append(TraceOp::Return(v));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn ic_hook_is_declared_for_future_integration() {
    // Even without an explicit IC op the emitter declares the
    // inline-cache hook so the host module can resolve it after
    // installation. We verify the function references the
    // SaveDeopt + ResolveCall imports at minimum; the IC hook is
    // declared but lazily linked.
    let mut b = TraceBuffer::new();
    let r = b.fresh_ssa();
    b.append(TraceOp::Call(r, FuncId(0), vec![], EffectClass::Pure));
    b.append(TraceOp::Return(r));
    let ctx = emit_and_verify(&b.into_optimized());
    // 3 imported user functions => fn0..fn2 in the dump.
    let s = format!("{}", ctx.func);
    assert!(s.contains("fn0"), "save_deopt import missing from {s}");
    assert!(s.contains("fn1"), "resolve_call import missing from {s}");
}

#[test]
fn ic_pattern_with_multiple_guards_verifies() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let receiver = b.fresh_ssa();
    b.append(TraceOp::ConstI64(arg, 7));
    b.append(TraceOp::ConstI64(receiver, 0xbeef));
    b.record_type(arg, ObservedType::I64);
    b.record_type(receiver, ObservedType::Ptr);

    let pc1 = b.append(TraceOp::Guard(
        GuardKind::TypeCheck(arg, ObservedType::I64),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc1,
        ExternalPc(0x1),
        GuardKind::TypeCheck(arg, ObservedType::I64),
    ));
    let pc2 = b.append(TraceOp::Guard(
        GuardKind::TypeCheck(receiver, ObservedType::Ptr),
        SsaVar::NONE,
    ));
    b.record_guard(GuardSite::new(
        pc2,
        ExternalPc(0x2),
        GuardKind::TypeCheck(receiver, ObservedType::Ptr),
    ));

    let r = b.fresh_ssa();
    b.append(TraceOp::Call(
        r,
        FuncId(33),
        vec![receiver, arg],
        EffectClass::ReadOnly,
    ));
    b.append(TraceOp::Return(r));
    emit_and_verify(&b.into_optimized());
}
