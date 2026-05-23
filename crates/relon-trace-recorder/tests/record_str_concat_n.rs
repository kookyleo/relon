//! #168: recorder-side end-to-end for `Op::StrConcatN`.
//!
//! The unit tests inside `lowering.rs::tests` exercise the pure
//! `lower_op` rule with synthesised SSA windows; this integration
//! file drives the full `RecorderState::record_op` path so the
//! recorder's operand-stack mirror + buffer append behaviour is
//! covered. Pre-#168 the same input produced
//! `RecordResult::Abort(AbortReason::UnsupportedOp("StrConcatN"))`;
//! today the recorder must emit a real `TraceOp::StrConcatN` with
//! `operand_count` operands and one `NotNull` guard per operand.

use relon_ir::Op;
use relon_trace_jit::{GuardKind, ObservedType, SsaVar, TraceOp};
use relon_trace_recorder::{RecordResult, RecorderState};

/// Push `n` const-i64 ops onto the recorder so the operand-stack mirror
/// contains `n` SSA cells, then return the SSA ids in push order
/// (`[0]` = first pushed / leftmost source arg, `[n-1]` = last pushed).
fn push_const_pointers(recorder: &mut RecorderState, n: u32) -> Vec<SsaVar> {
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        // Each const is a fake `*const StringRef` cast to i64; the
        // recorder's `Op::ConstI64` lowering allocates a fresh SSA and
        // pushes it onto the operand-stack mirror.
        let res = recorder.record_op(
            &Op::ConstI64(0x1000 + i as i64),
            &[],
            Some(ObservedType::I64),
        );
        match res {
            RecordResult::Ok { value: Some(ssa) } => out.push(ssa),
            other => panic!("ConstI64 must Ok with a fresh SSA, got {other:?}"),
        }
    }
    out
}

#[test]
fn record_str_concat_n_three_operands_emits_trace_op() {
    let mut r = RecorderState::new();
    let ssas = push_const_pointers(&mut r, 3);
    // Recording-time inputs mirror what the trace_recording walker
    // feeds: top-of-stack first → `inputs[0]` is the rhs / topmost.
    let inputs: Vec<SsaVar> = ssas.iter().rev().copied().collect();
    let res = r.record_op(
        &Op::StrConcatN { operand_count: 3 },
        &inputs,
        Some(ObservedType::Ptr),
    );
    assert!(
        matches!(
            res,
            RecordResult::Ok { value: Some(_) } | RecordResult::NeedsGuard { value: Some(_), .. }
        ),
        "StrConcatN no longer aborts post-#168; got {res:?}"
    );
    assert!(!r.is_aborted(), "recorder must not be in abort state");

    let buffer = r.finalize().expect("trace must finalise cleanly");
    let ops: Vec<&TraceOp> = buffer.ops.iter().collect();
    let concat_n = ops
        .iter()
        .find_map(|op| match op {
            TraceOp::StrConcatN { dst, operands } => Some((dst, operands.clone())),
            _ => None,
        })
        .expect("buffer must carry exactly one TraceOp::StrConcatN");
    let _ = concat_n.0;
    assert_eq!(
        concat_n.1.len(),
        3,
        "operand_count round-trips through the recorder"
    );
    // The lowering reverses the top-first `inputs` window into source
    // order so `operands[0]` is the leftmost / deepest leaf — matches
    // the IR-side `Op::StrConcatN` operand-stack semantics.
    assert_eq!(concat_n.1, ssas, "operands must run left-to-right");

    // Three NotNull guards in the stream, one per operand SSA. We don't
    // pin guard ordering (the recorder may emit them in any stable
    // order); just count.
    let notnull_count: usize = ops
        .iter()
        .filter(|op| {
            matches!(
                op,
                TraceOp::Guard {
                    kind: GuardKind::NotNull(_),
                    check: _
                }
            )
        })
        .count();
    assert_eq!(notnull_count, 3, "one NotNull guard per StrConcatN operand");
}

#[test]
fn record_str_concat_n_five_operands_aborts_over_cap() {
    let mut r = RecorderState::new();
    let ssas = push_const_pointers(&mut r, 5);
    let inputs: Vec<SsaVar> = ssas.iter().rev().copied().collect();
    let res = r.record_op(
        &Op::StrConcatN { operand_count: 5 },
        &inputs,
        Some(ObservedType::Ptr),
    );
    assert!(
        matches!(
            res,
            RecordResult::Abort(relon_trace_recorder::AbortReason::UnsupportedOp(
                "StrConcatNOverCap"
            ))
        ),
        "operand_count > MAX_INLINE_STR_CONCAT_N must abort cleanly so the \
         outer tier router can fall back to the cranelift AOT backend; \
         got {res:?}"
    );
}

#[test]
fn record_str_concat_n_two_operands_aborts_too_few() {
    // The IR fold pass only produces operand_count >= 3 (the two-
    // operand case stays as `Op::Add(IrType::String)`). A hand-built
    // fragment with operand_count = 2 should surface a distinct abort
    // reason for diagnostic clarity.
    let mut r = RecorderState::new();
    let ssas = push_const_pointers(&mut r, 2);
    let inputs: Vec<SsaVar> = ssas.iter().rev().copied().collect();
    let res = r.record_op(
        &Op::StrConcatN { operand_count: 2 },
        &inputs,
        Some(ObservedType::Ptr),
    );
    assert!(
        matches!(
            res,
            RecordResult::Abort(relon_trace_recorder::AbortReason::UnsupportedOp(
                "StrConcatNTooFewOperands"
            ))
        ),
        "operand_count < 3 must abort with the dedicated diagnostic; got {res:?}"
    );
}
