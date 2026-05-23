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
    b.append(TraceOp::ConstI64 { dst: arg, value: 5 });
    b.append(TraceOp::Call {
        dst: ret,
        func: FuncId(1),
        args: vec![arg],
        effect: EffectClass::Pure,
    });
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 1);
    // Guard op should sit immediately before the Call op.
    let guard_idx = b
        .ops
        .iter()
        .position(|o| matches!(o, TraceOp::Guard { kind: _, check: _ }))
        .expect("guard inserted");
    let call_idx = b
        .ops
        .iter()
        .position(|o| {
            matches!(
                o,
                TraceOp::Call {
                    dst: _,
                    func: _,
                    args: _,
                    effect: _
                }
            )
        })
        .expect("call present");
    assert_eq!(guard_idx + 1, call_idx);
}

#[test]
fn unrecoverable_call_is_left_untouched() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.record_type(arg, ObservedType::I32);
    b.append(TraceOp::ConstI32 { dst: arg, value: 1 });
    b.append(TraceOp::Call {
        dst: ret,
        func: FuncId(1),
        args: vec![arg],
        effect: EffectClass::Unrecoverable,
    });
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 0);
}

#[test]
fn no_type_info_means_no_guard() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: arg, value: 3 });
    b.append(TraceOp::Call {
        dst: ret,
        func: FuncId(2),
        args: vec![arg],
        effect: EffectClass::Pure,
    });
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
    b.append(TraceOp::ConstI32 { dst: arg, value: 1 });
    b.append(TraceOp::Call {
        dst: ret,
        func: FuncId(5),
        args: vec![arg],
        effect: EffectClass::ReadOnly,
    });
    TypeSpec.run(&mut b);
    assert_eq!(b.guard_count(), 1);
    let site = &b.guards[0];
    assert!(matches!(
        site.kind,
        GuardKind::TypeCheck(_, ObservedType::Bool)
    ));
    let op_at_pc = &b.ops[site.trace_pc as usize];
    assert!(matches!(op_at_pc, TraceOp::Guard { kind: _, check: _ }));
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
    b.append(TraceOp::ConstI64 {
        dst: arg1,
        value: 1,
    });
    b.append(TraceOp::Call {
        dst: r1,
        func: FuncId(1),
        args: vec![arg1],
        effect: EffectClass::Pure,
    });
    b.append(TraceOp::ConstI32 {
        dst: arg2,
        value: 2,
    });
    b.append(TraceOp::Call {
        dst: r2,
        func: FuncId(2),
        args: vec![arg2],
        effect: EffectClass::Pure,
    });
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 2);
    assert_eq!(b.guard_count(), 2);
}

#[test]
fn no_arg_call_skipped() {
    let mut b = TraceBuffer::new();
    let ret = b.fresh_ssa();
    b.append(TraceOp::Call {
        dst: ret,
        func: FuncId(3),
        args: vec![],
        effect: EffectClass::Pure,
    });
    let report = TypeSpec.run(&mut b);
    assert_eq!(report.guards_added, 0);
}
