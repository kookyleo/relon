//! EffectClass-driven abort paths.

use relon_ir::{IrType, Op};
use relon_trace_jit::EffectClass;
use relon_trace_recorder::{lower_op, AbortReason, OpLoweringContext};
use relon_trace_recorder::{RecordResult, RecorderState};

fn call_op() -> Op {
    Op::Call {
        fn_index: 0,
        arg_count: 0,
        param_tys: vec![],
        ret_ty: IrType::I64,
    }
}

#[test]
fn unrecoverable_effect_aborts() {
    let mut r = RecorderState::new();
    let res = r.record_op_with_call_effect(&call_op(), &[], None, EffectClass::Unrecoverable);
    assert!(matches!(
        res,
        RecordResult::Abort(AbortReason::UnrecoverableEffect)
    ));
}

#[test]
fn pure_effect_does_not_abort() {
    let mut r = RecorderState::new();
    let res = r.record_op_with_call_effect(&call_op(), &[], None, EffectClass::Pure);
    assert!(matches!(res, RecordResult::Ok { .. }));
    assert!(!r.is_aborted());
}

#[test]
fn read_only_effect_does_not_abort() {
    let mut r = RecorderState::new();
    let res = r.record_op_with_call_effect(&call_op(), &[], None, EffectClass::ReadOnly);
    assert!(matches!(res, RecordResult::Ok { .. }));
}

#[test]
fn recoverable_write_effect_does_not_abort() {
    let mut r = RecorderState::new();
    let res = r.record_op_with_call_effect(&call_op(), &[], None, EffectClass::RecoverableWrite);
    assert!(matches!(res, RecordResult::Ok { .. }));
}

#[test]
fn lower_op_unsupported_carries_variant_name() {
    let outcome = lower_op(
        &Op::ReadStringLen,
        OpLoweringContext::new(&[], relon_trace_jit::SsaVar(0)),
    );
    match outcome {
        relon_trace_recorder::lowering::LowerOutcome::Abort(AbortReason::UnsupportedOp(name)) => {
            assert_eq!(name, "ReadStringLen");
        }
        other => panic!("{:?}", other),
    }
}

#[test]
fn abort_is_sticky_across_record_calls() {
    let mut r = RecorderState::new();
    r.abort(AbortReason::TraceTooLong);
    let res = r.record_op(&Op::ConstI64(0), &[], None);
    assert!(matches!(
        res,
        RecordResult::Abort(AbortReason::TraceTooLong)
    ));
}

#[test]
fn first_abort_reason_wins() {
    let mut r = RecorderState::new();
    r.abort(AbortReason::UnrecoverableEffect);
    r.abort(AbortReason::TraceTooLong);
    assert_eq!(
        r.finalize().err().unwrap(),
        AbortReason::UnrecoverableEffect
    );
}
