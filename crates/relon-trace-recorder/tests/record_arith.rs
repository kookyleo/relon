//! Smoke tests for arithmetic op recording.

use relon_ir::{IrType, Op};
use relon_trace_jit::{EffectClass, GuardKind, ObservedType, TraceOp};
use relon_trace_recorder::{RecordResult, RecorderState};

fn const_i64(r: &mut RecorderState, v: i64) -> relon_trace_jit::SsaVar {
    match r.record_op(&Op::ConstI64(v), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(s) } => s,
        other => panic!("ConstI64({v}) -> {other:?}"),
    }
}

#[test]
fn add_chain_records_three_ops() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 1);
    let b = const_i64(&mut r, 2);
    let _ = r.record_op(&Op::Add(IrType::I64), &[b, a], Some(ObservedType::I64));
    let buf = r.finalize().expect("no abort");
    // 2 const + 1 add + 1 ArithOverflow guard. No TypeCheck guards
    // because each SSA var is observed for the first time.
    assert_eq!(buf.op_count(), 4);
}

#[test]
fn add_op_carries_pure_effect_class() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 1);
    let b = const_i64(&mut r, 2);
    let _ = r.record_op(&Op::Add(IrType::I64), &[b, a], None);
    let buf = r.finalize().unwrap();
    let add = buf
        .ops
        .iter()
        .find(|o| matches!(o, TraceOp::Add(_, _, _)))
        .expect("add present");
    assert_eq!(add.effect_class(), EffectClass::Pure);
}

#[test]
fn sub_then_mul_records_two_ops() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 10);
    let b = const_i64(&mut r, 4);
    let _ = r.record_op(&Op::Sub(IrType::I64), &[b, a], None);
    let c = const_i64(&mut r, 2);
    let _ = r.record_op(&Op::Mul(IrType::I64), &[c, a], None);
    let _ = b;
    let buf = r.finalize().unwrap();
    let arith = buf
        .ops
        .iter()
        .filter(|o| matches!(o, TraceOp::Sub(_, _, _) | TraceOp::Mul(_, _, _)))
        .count();
    assert_eq!(arith, 2);
}

#[test]
fn float_arith_aborts_with_unsupported_op() {
    let mut r = RecorderState::new();
    let _ = r.record_op(
        &Op::Add(IrType::F64),
        &[relon_trace_jit::SsaVar(0), relon_trace_jit::SsaVar(1)],
        None,
    );
    assert!(r.is_aborted());
    let err = r.finalize().expect_err("aborted");
    assert!(matches!(
        err,
        relon_trace_recorder::AbortReason::UnsupportedOp("FloatArith")
    ));
}

#[test]
fn div_emits_recoverable_write_class() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 6);
    let b = const_i64(&mut r, 2);
    let _ = r.record_op(&Op::Div(IrType::I64), &[b, a], None);
    let buf = r.finalize().unwrap();
    let div = buf
        .ops
        .iter()
        .find(|o| matches!(o, TraceOp::Div(_, _, _)))
        .expect("div present");
    assert_eq!(div.effect_class(), EffectClass::RecoverableWrite);
}

#[test]
fn arith_emits_overflow_guard() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 1);
    let b = const_i64(&mut r, 2);
    let _ = r.record_op(&Op::Add(IrType::I64), &[b, a], None);
    let buf = r.finalize().unwrap();
    let guard_count = buf
        .ops
        .iter()
        .filter(|o| matches!(o, TraceOp::Guard(GuardKind::ArithOverflow(_), _)))
        .count();
    assert_eq!(guard_count, 1);
}
