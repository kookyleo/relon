//! BrIf / Br / Loop / branch recording.

use relon_ir::Op;
use relon_trace_jit::{ObservedType, TraceOp};
use relon_trace_recorder::{RecordResult, RecorderState};

#[test]
fn br_if_emits_guard_op() {
    let mut r = RecorderState::new();
    let cond = match r.record_op(&Op::ConstBool(true), &[], Some(ObservedType::Bool)) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("{other:?}"),
    };
    let _ = r.record_op(&Op::BrIf { label_depth: 0 }, &[cond], None);
    let buf = r.finalize().unwrap();
    let guards = buf
        .ops
        .iter()
        .filter(|o| matches!(o, TraceOp::Guard(_, _)))
        .count();
    assert!(guards >= 1);
}

#[test]
fn br_is_side_effect_only_emits_nothing() {
    let mut r = RecorderState::new();
    let before = r.op_count();
    let _ = r.record_op(&Op::Br { label_depth: 0 }, &[], None);
    let after = r.op_count();
    assert_eq!(before, after, "Br should not emit a TraceOp");
}

#[test]
fn loop_op_emits_mark_loop_head() {
    let mut r = RecorderState::new();
    let _ = r.record_op(
        &Op::Loop {
            result_ty: None,
            body: vec![],
        },
        &[],
        None,
    );
    let buf = r.finalize().unwrap();
    let head_count = buf
        .ops
        .iter()
        .filter(|o| matches!(o, TraceOp::MarkLoopHead { .. }))
        .count();
    assert_eq!(head_count, 1);
}

#[test]
fn br_table_aborts_unsupported() {
    let mut r = RecorderState::new();
    let _ = r.record_op(
        &Op::BrTable {
            default: 0,
            targets: vec![0, 1],
        },
        &[],
        None,
    );
    assert!(r.is_aborted());
    assert!(matches!(
        r.finalize().err().unwrap(),
        relon_trace_recorder::AbortReason::UnsupportedOp("BrTable")
    ));
}

#[test]
fn block_op_does_not_emit_trace_op() {
    let mut r = RecorderState::new();
    let _ = r.record_op(
        &Op::Block {
            result_ty: None,
            body: vec![],
        },
        &[],
        None,
    );
    let buf = r.finalize().unwrap();
    assert_eq!(buf.op_count(), 0);
}

#[test]
fn nested_loop_markers_get_unique_ids() {
    let mut r = RecorderState::new();
    let _ = r.record_op(
        &Op::Loop {
            result_ty: None,
            body: vec![],
        },
        &[],
        None,
    );
    let _ = r.record_op(
        &Op::Loop {
            result_ty: None,
            body: vec![],
        },
        &[],
        None,
    );
    let buf = r.finalize().unwrap();
    let mut ids: Vec<u32> = buf
        .ops
        .iter()
        .filter_map(|o| match o {
            TraceOp::MarkLoopHead { loop_id } => Some(*loop_id),
            _ => None,
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1]);
}
