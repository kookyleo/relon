//! Type-specialisation pass smoke tests.

use relon_trace_jit::optimizer::type_spec::TypeSpec;
use relon_trace_jit::{
    EffectClass, FuncId, GuardKind, ObservedType, OptimizerPass, TraceBuffer, TraceOp,
};

#[test]
fn pure_call_with_observed_type_gets_guard() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.record_type(arg, ObservedType::I64);
    b.append(TraceOp::ConstI64(arg, 5));
    b.append(TraceOp::Call(ret, FuncId(1), vec![arg], EffectClass::Pure));
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 1);
    // Guard op should sit immediately before the Call op.
    let guard_idx = b
        .ops
        .iter()
        .position(|o| matches!(o, TraceOp::Guard(_, _)))
        .expect("guard inserted");
    let call_idx = b
        .ops
        .iter()
        .position(|o| matches!(o, TraceOp::Call(_, _, _, _)))
        .expect("call present");
    assert_eq!(guard_idx + 1, call_idx);
}

#[test]
fn unrecoverable_call_is_left_untouched() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.record_type(arg, ObservedType::I32);
    b.append(TraceOp::ConstI32(arg, 1));
    b.append(TraceOp::Call(
        ret,
        FuncId(1),
        vec![arg],
        EffectClass::Unrecoverable,
    ));
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 0);
}

#[test]
fn no_type_info_means_no_guard() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.append(TraceOp::ConstI64(arg, 3));
    b.append(TraceOp::Call(ret, FuncId(2), vec![arg], EffectClass::Pure));
    // No record_type call -> nothing to specialise on.
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 0);
}

#[test]
fn guardsite_anchored_at_inserted_guard() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.record_type(arg, ObservedType::Bool);
    b.append(TraceOp::ConstI32(arg, 1));
    b.append(TraceOp::Call(
        ret,
        FuncId(5),
        vec![arg],
        EffectClass::ReadOnly,
    ));
    TypeSpec.run(&mut b);
    assert_eq!(b.guard_count(), 1);
    let site = &b.guards[0];
    assert!(matches!(
        site.kind,
        GuardKind::TypeCheck(_, ObservedType::Bool)
    ));
    let op_at_pc = &b.ops[site.trace_pc as usize];
    assert!(matches!(op_at_pc, TraceOp::Guard(_, _)));
}

#[test]
fn two_calls_get_two_guards() {
    let mut b = TraceBuffer::new();
    let arg1 = b.fresh_ssa();
    let arg2 = b.fresh_ssa();
    let r1 = b.fresh_ssa();
    let r2 = b.fresh_ssa();
    b.record_type(arg1, ObservedType::I64);
    b.record_type(arg2, ObservedType::I32);
    b.append(TraceOp::ConstI64(arg1, 1));
    b.append(TraceOp::Call(r1, FuncId(1), vec![arg1], EffectClass::Pure));
    b.append(TraceOp::ConstI32(arg2, 2));
    b.append(TraceOp::Call(r2, FuncId(2), vec![arg2], EffectClass::Pure));
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 2);
    assert_eq!(b.guard_count(), 2);
}

#[test]
fn no_arg_call_skipped() {
    let mut b = TraceBuffer::new();
    let ret = b.fresh_ssa();
    b.append(TraceOp::Call(ret, FuncId(3), vec![], EffectClass::Pure));
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 0);
}
