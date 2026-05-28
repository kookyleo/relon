//! W5 IV-overflow-elim per-phi behaviour test.
//!
//! W5's loop carries 4 phis (KEY_IDX, KEY_PTR, count, i). Only the
//! counter `i + 1` qualifies for overflow-guard removal under the
//! `MAX_SAFE_LOOP_BOUND` proof:
//! - KEY_IDX next is `Mod` (not `Add`), skipped
//! - KEY_PTR next is `ListGet` (not `Add`), skipped
//! - count next is `Add { count, dict_value }`, dict_value is i64
//!   (not Bool / not const ≤ 1), skipped
//! - i next is `Add { i, ConstI64(1) }`, qualifies
//!
//! The pass must drop exactly **one** ArithOverflow guard (i + 1) and
//! splice an entry guard, leaving the count and Mod guards intact.
//! Pre-2026-05-25 "all-or-nothing" behaviour returned `None` on the
//! count step's check failure and didn't strip i — this test pins the
//! per-phi refactor that does.

use relon_codegen_cranelift::{RecordingOutcome, TraceRecordingEvaluator};
use relon_ir::ir::{IrType, Op, TaggedOp};
use relon_ir::shape_hash::shape_hash_for_keys;
use relon_parser::TokenRange;
use relon_trace_jit::optimizer::OptimizerPipeline;
use relon_trace_jit::runtime::{build_dict_record_v2, build_flat_list_record, build_string_record};
use relon_trace_jit::trace_ir::{GuardKind, TraceOp};
use relon_trace_recorder::RecorderState;

fn tag(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

fn build_w5_body(shape_hash: u64, record_len: u32) -> Vec<TaggedOp> {
    const I: u32 = 0;
    const ACC: u32 = 1;
    const KEY_IDX: u32 = 2;
    const KEY_PTR: u32 = 3;
    vec![
        tag(Op::ConstI64(0)),
        tag(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        tag(Op::ConstI64(0)),
        tag(Op::LetSet {
            idx: ACC,
            ty: IrType::I64,
        }),
        tag(Op::Block {
            result_ty: None,
            body: vec![tag(Op::Loop {
                result_ty: None,
                body: vec![
                    tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    tag(Op::LocalGet(0)),
                    tag(Op::Ge(IrType::I64)),
                    tag(Op::BrIf { label_depth: 1 }),
                    tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    tag(Op::ConstI64(10)),
                    tag(Op::Mod(IrType::I64)),
                    tag(Op::LetSet {
                        idx: KEY_IDX,
                        ty: IrType::I64,
                    }),
                    tag(Op::LocalGet(2)),
                    tag(Op::LetGet {
                        idx: KEY_IDX,
                        ty: IrType::I64,
                    }),
                    tag(Op::ListGetByIntIdx {
                        element_ty: IrType::I64,
                    }),
                    tag(Op::LetSet {
                        idx: KEY_PTR,
                        ty: IrType::I64,
                    }),
                    tag(Op::LocalGet(1)),
                    tag(Op::LetGet {
                        idx: KEY_PTR,
                        ty: IrType::I64,
                    }),
                    tag(Op::DictGetByStringKey {
                        shape_hash,
                        value_ty: IrType::I64,
                        entry_count_hint: Some(10),
                        record_len_hint: Some(record_len),
                    }),
                    tag(Op::LetGet {
                        idx: ACC,
                        ty: IrType::I64,
                    }),
                    tag(Op::Add(IrType::I64)),
                    tag(Op::LetSet {
                        idx: ACC,
                        ty: IrType::I64,
                    }),
                    tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    tag(Op::ConstI64(1)),
                    tag(Op::Add(IrType::I64)),
                    tag(Op::LetSet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    tag(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        tag(Op::LetGet {
            idx: ACC,
            ty: IrType::I64,
        }),
        tag(Op::Return),
    ]
}

fn dump_ops(label: &str, ops: &[TraceOp]) {
    println!("=== {label} ({n} ops) ===", n = ops.len());
    for (pc, op) in ops.iter().enumerate() {
        println!("  {pc:>3}: {op:?}");
    }
}

#[test]
fn w5_loop_arith_overflow_guards_diag() {
    let labels = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
    let key_records: Vec<Vec<u8>> = labels.iter().map(|s| build_string_record(s)).collect();
    let shape_hash = shape_hash_for_keys(labels.iter().copied());
    let entries: Vec<(&[u8], i64)> = labels
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_bytes(), (i as i64) + 1))
        .collect();
    let dict_bytes = build_dict_record_v2(shape_hash, &entries);
    let key_record_ptrs: Vec<i64> = key_records.iter().map(|kr| kr.as_ptr() as i64).collect();
    let keys_list_bytes = build_flat_list_record(&key_record_ptrs);

    let args = vec![
        (10u64, IrType::I64),
        (dict_bytes.as_ptr() as u64, IrType::I64),
        (keys_list_bytes.as_ptr() as u64, IrType::I64),
    ];

    let body = build_w5_body(shape_hash, dict_bytes.len() as u32);
    let mut recorder = RecorderState::new();
    let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &args, &body);
    let buf = match outcome {
        RecordingOutcome::Recorded { recorder, .. } => recorder.buffer().clone(),
        RecordingOutcome::Aborted { reason, .. } => {
            panic!("recorder aborted on W5 body: {reason:?}");
        }
    };

    dump_ops("W5 post-record", &buf.ops);

    let mut buf_after = buf.clone();
    let arith_overflow_before = buf_after
        .ops
        .iter()
        .filter(|op| {
            matches!(
                op,
                TraceOp::Guard {
                    kind: GuardKind::ArithOverflow(_),
                    ..
                }
            )
        })
        .count();
    let reports = OptimizerPipeline::default_pipeline().run(&mut buf_after);
    let arith_overflow_after = buf_after
        .ops
        .iter()
        .filter(|op| {
            matches!(
                op,
                TraceOp::Guard {
                    kind: GuardKind::ArithOverflow(_),
                    ..
                }
            )
        })
        .count();

    dump_ops("W5 post-pipeline", &buf_after.ops);
    println!("pipeline reports: {reports:#?}");
    println!("ArithOverflow guards: before={arith_overflow_before} after={arith_overflow_after}");

    // IV pass strips two ArithOverflow guards on W5:
    //   1. i + 1 (counter, per-phi IV proof, entry guard inserted)
    //   2. Mod(i, 10) (const +divisor ≠ -1, statically safe)
    // count's accumulator step is i64 dict value (not Bool/const) so
    // count's overflow guard stays.
    assert!(
        arith_overflow_before >= 2,
        "expected ≥ 2 ArithOverflow guards pre-pipeline (Mod, count+value, i+1), got {arith_overflow_before}"
    );
    assert_eq!(
        arith_overflow_before - arith_overflow_after,
        2,
        "IV pass must strip exactly two ArithOverflow guards on W5 (i + 1, Mod 10), leaving count's intact"
    );

    // The entry guard triple (Cmp Gt + Guard IsZero) should be spliced
    // ahead of the MarkLoopHead. Mirror w4_iv_overflow_elim_smoke shape.
    let head_pc = buf_after
        .ops
        .iter()
        .position(|op| matches!(op, TraceOp::MarkLoopHead { .. }))
        .expect("loop head must still be present");
    assert!(head_pc >= 3, "entry-guard triple expects three op slots");
    match &buf_after.ops[head_pc - 2] {
        TraceOp::Cmp { kind, .. } => assert_eq!(
            *kind,
            relon_trace_jit::trace_ir::CmpKind::Gt,
            "entry-guard Cmp must be Gt"
        ),
        other => panic!("expected Cmp Gt before MarkLoopHead, got {other:?}"),
    }
    assert!(matches!(
        &buf_after.ops[head_pc - 1],
        TraceOp::Guard {
            kind: GuardKind::IsZero(_),
            ..
        }
    ));
}
