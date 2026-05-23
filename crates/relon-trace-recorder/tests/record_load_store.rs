//! Smoke tests for LoadField / StoreField recording.

use relon_ir::{IrType, Op};
use relon_trace_jit::{EffectClass, ObservedType, SsaVar, TraceOp};
use relon_trace_recorder::{RecordResult, RecorderState};

#[test]
fn load_field_emits_load_op() {
    let mut r = RecorderState::new();
    // Push a "base pointer" SSA first via a const.
    let base = match r.record_op(&Op::ConstI64(0x1000), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let _ = r.record_op(
        &Op::LoadField {
            offset: 16,
            ty: IrType::I64,
        },
        &[base],
        Some(ObservedType::I64),
    );
    let buf = r.finalize().unwrap();
    let load = buf
        .ops
        .iter()
        .find(|o| {
            matches!(
                o,
                TraceOp::Load {
                    dst: _,
                    base: _,
                    offset: _
                }
            )
        })
        .expect("load present");
    assert_eq!(load.effect_class(), EffectClass::ReadOnly);
}

#[test]
fn load_emits_not_null_guard_before() {
    let mut r = RecorderState::new();
    let base = match r.record_op(&Op::ConstI64(0x2000), &[], None) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let _ = r.record_op(
        &Op::LoadField {
            offset: 0,
            ty: IrType::I32,
        },
        &[base],
        None,
    );
    let buf = r.finalize().unwrap();
    // Schema-bounded offset → only a non-null base check at runtime.
    let not_null = buf
        .ops
        .iter()
        .filter(|o| {
            matches!(
                o,
                TraceOp::Guard {
                    kind: relon_trace_jit::GuardKind::NotNull(_),
                    check: _
                }
            )
        })
        .count();
    assert!(not_null >= 1, "expected at least one NotNull guard");
}

#[test]
fn store_field_marks_recoverable_write() {
    let mut r = RecorderState::new();
    let value = match r.record_op(&Op::ConstI64(7), &[], None) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let base = match r.record_op(&Op::ConstI64(0x1000), &[], None) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let _ = r.record_op(
        &Op::StoreField {
            offset: 0,
            ty: IrType::I64,
        },
        &[value, base],
        None,
    );
    let buf = r.finalize().unwrap();
    let store = buf
        .ops
        .iter()
        .find(|o| {
            matches!(
                o,
                TraceOp::Store {
                    base: _,
                    offset: _,
                    src: _
                }
            )
        })
        .expect("store present");
    assert_eq!(store.effect_class(), EffectClass::RecoverableWrite);
}

#[test]
fn missing_base_uses_sentinel_and_skips_guard() {
    // LoadField with no inputs uses SsaVar::NONE as base; the recorder
    // suppresses the NotNull guard in that case (no valid SSA id to
    // gate against).
    let mut r = RecorderState::new();
    let _ = r.record_op(
        &Op::LoadField {
            offset: 0,
            ty: IrType::I64,
        },
        &[],
        None,
    );
    let buf = r.finalize().unwrap();
    let not_null = buf
        .ops
        .iter()
        .filter(|o| {
            matches!(
                o,
                TraceOp::Guard {
                    kind: relon_trace_jit::GuardKind::NotNull(_),
                    check: _
                }
            )
        })
        .count();
    assert_eq!(not_null, 0);
}

#[test]
fn load_then_store_chains_two_memory_ops() {
    let mut r = RecorderState::new();
    let base = match r.record_op(&Op::ConstI64(0x1000), &[], None) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let loaded = match r.record_op(
        &Op::LoadField {
            offset: 0,
            ty: IrType::I64,
        },
        &[base],
        None,
    ) {
        RecordResult::Ok { value: Some(v) } => v,
        RecordResult::NeedsGuard { value: Some(v), .. } => v,
        other => panic!("{other:?}"),
    };
    let _ = r.record_op(
        &Op::StoreField {
            offset: 8,
            ty: IrType::I64,
        },
        &[loaded, base],
        None,
    );
    let buf = r.finalize().unwrap();
    let loads = buf
        .ops
        .iter()
        .filter(|o| {
            matches!(
                o,
                TraceOp::Load {
                    dst: _,
                    base: _,
                    offset: _
                }
            )
        })
        .count();
    let stores = buf
        .ops
        .iter()
        .filter(|o| {
            matches!(
                o,
                TraceOp::Store {
                    base: _,
                    offset: _,
                    src: _
                }
            )
        })
        .count();
    assert_eq!(loads, 1);
    assert_eq!(stores, 1);
}

#[test]
fn unsupported_list_load_aborts() {
    let mut r = RecorderState::new();
    let _ = r.record_op(&Op::LoadStringPtr { offset: 0 }, &[SsaVar(0)], None);
    assert!(r.is_aborted());
    assert!(matches!(
        r.finalize().err().unwrap(),
        relon_trace_recorder::AbortReason::UnsupportedOp("LoadStringPtr")
    ));
}
