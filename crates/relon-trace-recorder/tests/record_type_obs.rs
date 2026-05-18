//! ObservedType inference + TypeCheck guard policy.

use ordered_float::OrderedFloat;
use relon_eval_api::Value;
use relon_ir::{IrType, Op};
use relon_trace_jit::{GuardKind, ObservedType, TraceOp};
use relon_trace_recorder::{infer_observed_type, AbortReason, RecordResult, RecorderState};

#[test]
fn infer_int_value() {
    assert_eq!(infer_observed_type(&Value::Int(42)), ObservedType::I64);
}

#[test]
fn infer_float_value() {
    assert_eq!(
        infer_observed_type(&Value::Float(OrderedFloat(1.0))),
        ObservedType::F64
    );
}

#[test]
fn infer_bool_value() {
    assert_eq!(infer_observed_type(&Value::Bool(true)), ObservedType::Bool);
}

#[test]
fn infer_string_is_ptr() {
    assert_eq!(
        infer_observed_type(&Value::String("hi".into())),
        ObservedType::Ptr
    );
}

#[test]
fn infer_null_is_ptr() {
    assert_eq!(infer_observed_type(&Value::Null), ObservedType::Ptr);
}

#[test]
fn first_observation_no_guard() {
    let mut r = RecorderState::new();
    let res = r.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64));
    assert!(matches!(res, RecordResult::Ok { .. }));
    let buf = r.finalize().unwrap();
    let guards = buf
        .ops
        .iter()
        .filter(|o| matches!(o, TraceOp::Guard(GuardKind::TypeCheck(_, _), _)))
        .count();
    assert_eq!(guards, 0);
}

#[test]
fn second_observation_emits_type_guard() {
    let mut r = RecorderState::new();
    let v = match r.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    // Re-observe the same SSA via a LetSet/LetGet round trip is the
    // canonical path; here we simulate it by reading through a Let
    // slot bound to the same SSA.
    let _ = r.record_op(
        &Op::LetSet {
            idx: 0,
            ty: IrType::I64,
        },
        &[v],
        None,
    );
    let res = r.record_op(
        &Op::LetGet {
            idx: 0,
            ty: IrType::I64,
        },
        &[],
        Some(ObservedType::I64),
    );
    match res {
        RecordResult::NeedsGuard { guard, .. } => {
            assert!(matches!(guard, GuardKind::TypeCheck(_, ObservedType::I64)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn type_mismatch_aborts() {
    let mut r = RecorderState::new();
    let v = match r.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    // Bind it to a let-slot, then read it back with a *different*
    // observed type — this is the hetero-typed re-observation case.
    let _ = r.record_op(
        &Op::LetSet {
            idx: 0,
            ty: IrType::I64,
        },
        &[v],
        None,
    );
    let res = r.record_op(
        &Op::LetGet {
            idx: 0,
            ty: IrType::I64,
        },
        &[],
        Some(ObservedType::F64),
    );
    // After mismatch the recorder is aborted; the call itself may
    // return Ok with the looked-up var, but is_aborted is sticky.
    let _ = res;
    assert!(r.is_aborted());
    assert!(matches!(
        r.finalize().err().unwrap(),
        AbortReason::GuardFailureInRecording
    ));
}

#[test]
fn duplicate_guard_is_deduped() {
    // Reading the same let-slot back twice with the same type should
    // emit at most one TypeCheck guard (the recorder dedupes).
    let mut r = RecorderState::new();
    let v = match r.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let _ = r.record_op(
        &Op::LetSet {
            idx: 0,
            ty: IrType::I64,
        },
        &[v],
        None,
    );
    let _ = r.record_op(
        &Op::LetGet {
            idx: 0,
            ty: IrType::I64,
        },
        &[],
        Some(ObservedType::I64),
    );
    let _ = r.record_op(
        &Op::LetGet {
            idx: 0,
            ty: IrType::I64,
        },
        &[],
        Some(ObservedType::I64),
    );
    let buf = r.finalize().unwrap();
    let guards = buf
        .ops
        .iter()
        .filter(|o| {
            matches!(
                o,
                TraceOp::Guard(GuardKind::TypeCheck(_, ObservedType::I64), _)
            )
        })
        .count();
    assert_eq!(guards, 1);
}
