//! Op::Call / CallNative / CallClosure recording.

use relon_ir::{IrType, Op};
use relon_trace_jit::{EffectClass, FuncId, TraceOp};
use relon_trace_recorder::{AbortReason, RecordResult, RecorderState};

fn call_op(fn_index: u32) -> Op {
    Op::Call {
        fn_index,
        arg_count: 2,
        param_tys: vec![IrType::I64, IrType::I64],
        ret_ty: IrType::I64,
    }
}

#[test]
fn pure_call_records_with_override() {
    let mut r = RecorderState::new();
    let a = match r.record_op(&Op::ConstI64(1), &[], None) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let b = match r.record_op(&Op::ConstI64(2), &[], None) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let res = r.record_op_with_call_effect(&call_op(42), &[a, b], None, EffectClass::Pure);
    match res {
        RecordResult::Ok { value: Some(_) } => {}
        other => panic!("{other:?}"),
    }
    let buf = r.finalize().unwrap();
    let call = buf
        .ops
        .iter()
        .find(|o| matches!(o, TraceOp::Call(_, FuncId(42), _, _)))
        .expect("call op");
    assert_eq!(call.effect_class(), EffectClass::Pure);
}

#[test]
fn read_only_call_records() {
    let mut r = RecorderState::new();
    let _ = r.record_op_with_call_effect(&call_op(7), &[], None, EffectClass::ReadOnly);
    let buf = r.finalize().unwrap();
    let call = buf
        .ops
        .iter()
        .find(|o| matches!(o, TraceOp::Call(_, _, _, _)))
        .unwrap();
    assert_eq!(call.effect_class(), EffectClass::ReadOnly);
}

#[test]
fn recoverable_write_call_records() {
    let mut r = RecorderState::new();
    let _ = r.record_op_with_call_effect(&call_op(8), &[], None, EffectClass::RecoverableWrite);
    let buf = r.finalize().unwrap();
    let call = buf
        .ops
        .iter()
        .find(|o| matches!(o, TraceOp::Call(_, _, _, _)))
        .unwrap();
    assert_eq!(call.effect_class(), EffectClass::RecoverableWrite);
}

#[test]
fn unrecoverable_call_aborts_even_with_override() {
    // F-D7 specializes stdlib indices 6 (concat) and 9 (substring)
    // onto the dedicated `TraceOp::Str*` fast path before the effect
    // check kicks in, so this regression test uses an index outside
    // the specialised set to keep exercising the
    // `UnrecoverableEffect` abort gate.
    let mut r = RecorderState::new();
    let res = r.record_op_with_call_effect(&call_op(100), &[], None, EffectClass::Unrecoverable);
    assert!(matches!(
        res,
        RecordResult::Abort(AbortReason::UnrecoverableEffect)
    ));
    assert!(r.is_aborted());
}

#[test]
fn call_without_override_aborts() {
    let mut r = RecorderState::new();
    let res = r.record_op(&call_op(11), &[], None);
    assert!(matches!(
        res,
        RecordResult::Abort(AbortReason::UnrecoverableEffect)
    ));
}

#[test]
fn call_native_always_aborts() {
    let mut r = RecorderState::new();
    let res = r.record_op(
        &Op::CallNative {
            import_idx: 0,
            param_tys: vec![],
            ret_ty: IrType::I64,
            cap_bit: 0,
        },
        &[],
        None,
    );
    assert!(matches!(
        res,
        RecordResult::Abort(AbortReason::UnrecoverableEffect)
    ));
}
