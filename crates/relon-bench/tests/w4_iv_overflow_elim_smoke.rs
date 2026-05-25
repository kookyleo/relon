//! W4 induction-variable overflow elimination smoke test.
//!
//! Builds the W4 hot-loop IR fixture, drives it through the production
//! recorder, runs the optimizer pipeline (including the new
//! `iv_overflow_elim` pass), and asserts the resulting trace stream has
//! zero `Guard(ArithOverflow(_))` ops left inside the loop body. Used
//! to verify the pass actually fires on the real workload (not just
//! the hand-built unit-test fixtures).
//!
//! Output: a side-by-side dump of the post-LICM and post-IvOverflowElim
//! op streams is printed via `println!` so `cargo test -- --nocapture`
//! shows the rewrite at a glance. Keep this dump pinned through the
//! lifetime of the pass so any future regression that re-introduces
//! the ArithOverflow guards on W4 surfaces on the next test run.

use relon_codegen_native::{RecordingOutcome, TraceRecordingEvaluator};
use relon_ir::ir::{IrType, Op, TaggedOp};
use relon_parser::TokenRange;
use relon_trace_jit::optimizer::OptimizerPipeline;
use relon_trace_jit::trace_ir::{GuardKind, TraceOp};
use relon_trace_recorder::RecorderState;

fn tag(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

/// Mirror of `relon_bench/benches/cmp_lua.rs::w4_recorder_body`. Kept
/// inline here so the test does not depend on the bench module
/// surface — benches aren't `pub`.
fn w4_recorder_body() -> Vec<TaggedOp> {
    use relon_trace_recorder::lowering::STDLIB_IDX_CONTAINS;
    const I: u32 = 0;
    const COUNT: u32 = 1;
    vec![
        tag(Op::ConstI64(0)),
        tag(Op::LetSet {
            idx: I,
            ty: IrType::I64,
        }),
        tag(Op::ConstI64(0)),
        tag(Op::LetSet {
            idx: COUNT,
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
                    tag(Op::LocalGet(1)),
                    tag(Op::LocalGet(2)),
                    tag(Op::Call {
                        fn_index: STDLIB_IDX_CONTAINS,
                        arg_count: 2,
                        param_tys: vec![IrType::String, IrType::String],
                        ret_ty: IrType::Bool,
                    }),
                    tag(Op::LetGet {
                        idx: COUNT,
                        ty: IrType::I64,
                    }),
                    tag(Op::Add(IrType::I64)),
                    tag(Op::LetSet {
                        idx: COUNT,
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
            idx: COUNT,
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
fn w4_loop_drops_arith_overflow_guards_after_pipeline() {
    // Wire stable `*const StringRef` pointers — `from_static_permanent`
    // leaks the header so the recorder's `__relon_str_contains` shim
    // can deref it without the trace string-arena reclaiming it
    // mid-call.
    use relon_trace_jit::runtime::StringRef;
    let haystack = StringRef::from_static_permanent("axb");
    let needle = StringRef::from_static_permanent("x");

    let args = vec![
        (1u64, IrType::I64),
        (haystack as u64, IrType::String),
        (needle as u64, IrType::String),
    ];

    let body = w4_recorder_body();
    let mut recorder = RecorderState::new();
    let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &args, &body);
    let buf = match outcome {
        RecordingOutcome::Recorded { recorder, .. } => recorder.buffer().clone(),
        RecordingOutcome::Aborted { reason, .. } => {
            panic!("recorder aborted on W4 body: {reason:?}");
        }
    };

    // Snapshot the post-record op stream BEFORE the pipeline so the
    // diff is readable when the test is run with `--nocapture`.
    dump_ops("W4 post-record", &buf.ops);

    // Run the production optimizer pipeline. We measure the impact of
    // `iv_overflow_elim` by counting overflow guards before and after.
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

    dump_ops("W4 post-pipeline", &buf_after.ops);
    println!("pipeline reports: {reports:#?}");
    println!(
        "ArithOverflow guards: before={arith_overflow_before} after={arith_overflow_after}"
    );

    assert!(
        arith_overflow_before >= 2,
        "expected at least two ArithOverflow guards in W4 pre-pipeline trace (count += hit, i += 1), got {arith_overflow_before}"
    );
    assert_eq!(
        arith_overflow_after, 0,
        "iv_overflow_elim must drop every ArithOverflow guard on the W4 trace"
    );

    // The entry guard `Cmp Gt` + `Guard IsZero` should now sit above
    // the `MarkLoopHead`. Sanity check the surrounding shape: at least
    // one `Cmp` op with `Gt` kind anchored against an SSA that's also
    // read by a subsequent `Guard(IsZero)`.
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
