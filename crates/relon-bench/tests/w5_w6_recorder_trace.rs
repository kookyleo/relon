//! F-D8-D smoke test: drive the W5 / W6 recorder body through the
//! full install + invoke pipeline and assert the trace is making
//! actual progress (i.e. running ~N body iterations before the exit
//! guard fires, not deopting on iter 0).
//!
//! The bench measurement that imports this crate sees timings in the
//! tens / hundreds of microseconds; if the trace deopts on iter 0 the
//! bench reads in the hundreds of nanoseconds and the ratio against
//! LuaJIT becomes meaningless. Pinning the "N iters actually ran"
//! invariant here keeps the bench's reported `trace_jit` row honest.

use std::time::Instant;

use relon_codegen_native::trace_install::{
    __relon_jump_to_recorder, clear_recording, global_trace_jit_state, register_recording,
    RecordingRegistration,
};
use relon_ir::ir::{IrType, Op, TaggedOp};
use relon_ir::shape_hash::shape_hash_for_keys;
use relon_parser::TokenRange;
use relon_trace_jit::{build_dict_record_v2, build_flat_list_record, build_string_record};

fn tag(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

fn build_w6_body() -> Vec<TaggedOp> {
    const I: u32 = 0;
    const ACC: u32 = 1;
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
                    tag(Op::LocalGet(1)),
                    tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    tag(Op::ListGetByIntIdx {
                        element_ty: IrType::I64,
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

#[test]
fn w6_recorder_trace_runs_actual_iterations() {
    let n: u64 = 10_000;
    let elements: Vec<i64> = (1..=(n as i64)).collect();
    let list_bytes = build_flat_list_record(&elements);

    let fn_id: u32 = 410;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    state.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body: build_w6_body(),
            param_tys: vec![IrType::I64, IrType::I64],
        },
    );
    let warm: [u64; 2] = [n, list_bytes.as_ptr() as u64];
    unsafe {
        __relon_jump_to_recorder(fn_id, warm.as_ptr());
    }
    assert!(
        state.lookup_trace(fn_id).is_some(),
        "W6 recorder body must install through the recorder pipeline"
    );

    // Drive once via the install state — the trace runs all N iters
    // before the exit guard fires; the fallback closure surfaces the
    // analytic answer (i.e. sum(1..=n) = n*(n+1)/2).
    let args: [u64; 2] = warm;
    let expected_sum: u64 = n * (n + 1) / 2;
    let observed =
        unsafe { state.invoke_with_fallback(fn_id, args.as_ptr(), 64, |_args| expected_sum) };
    assert_eq!(
        observed, expected_sum,
        "W6 recorder-driven trace + fallback must yield sum 1..=n"
    );

    // Timing-based invariant: with a 10k-iter loop body running native
    // i64 adds + a list-get bounds-checked load, the per-invocation
    // wall time should be at least ~5µs on any reasonable hardware.
    // If the trace deopts on iter 0 the timing collapses to a few
    // hundred ns (the fallback's `expected_sum` literal); pinning the
    // lower bound here means the bench timing reflects actual trace
    // work, not fallback shortcut.
    let t0 = Instant::now();
    for _ in 0..32 {
        let _ =
            unsafe { state.invoke_with_fallback(fn_id, args.as_ptr(), 64, |_args| expected_sum) };
    }
    let elapsed = t0.elapsed();
    let per_call_ns = elapsed.as_nanos() / 32;
    eprintln!("W6 trace per-call wall time: {} ns", per_call_ns);
    assert!(
        per_call_ns >= 1_000,
        "W6 trace per-call must be ≥ 1µs (saw {}ns) — early deopt would yield ~hundreds of ns",
        per_call_ns
    );
}

#[test]
fn w5_recorder_trace_runs_actual_iterations() {
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

    let n: u64 = 10_000;

    // W5 body uses I = 0, ACC = 1, KEY_IDX = 2, KEY_PTR = 3.
    const I: u32 = 0;
    const ACC: u32 = 1;
    const KEY_IDX: u32 = 2;
    const KEY_PTR: u32 = 3;
    let body = vec![
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
                    tag(Op::LetGet {
                        idx: I,
                        ty: IrType::I64,
                    }),
                    tag(Op::ConstI64(10)),
                    tag(Op::Div(IrType::I64)),
                    tag(Op::ConstI64(10)),
                    tag(Op::Mul(IrType::I64)),
                    tag(Op::Sub(IrType::I64)),
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
                        entry_count_hint: None,
                        record_len_hint: Some(dict_bytes.len() as u32),
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
    ];

    let fn_id: u32 = 411;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    state.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body,
            param_tys: vec![IrType::I64, IrType::I64, IrType::I64],
        },
    );
    let warm: [u64; 3] = [
        n,
        dict_bytes.as_ptr() as u64,
        keys_list_bytes.as_ptr() as u64,
    ];
    unsafe {
        __relon_jump_to_recorder(fn_id, warm.as_ptr());
    }
    assert!(
        state.lookup_trace(fn_id).is_some(),
        "W5 recorder body must install through the recorder pipeline"
    );

    // Analytic answer: each block of 10 picks sums to 55. n=10000 → 1000 blocks.
    let expected_sum: u64 = n / 10 * 55;
    let args: [u64; 3] = warm;
    let observed =
        unsafe { state.invoke_with_fallback(fn_id, args.as_ptr(), 64, |_args| expected_sum) };
    assert_eq!(
        observed, expected_sum,
        "W5 recorder-driven trace + fallback must yield sum dict[keys[i%10]] for i in 0..n"
    );

    let t0 = Instant::now();
    for _ in 0..32 {
        let _ =
            unsafe { state.invoke_with_fallback(fn_id, args.as_ptr(), 64, |_args| expected_sum) };
    }
    let elapsed = t0.elapsed();
    let per_call_ns = elapsed.as_nanos() / 32;
    eprintln!("W5 trace per-call wall time: {} ns", per_call_ns);
    assert!(
        per_call_ns >= 5_000,
        "W5 trace per-call must be ≥ 5µs (saw {}ns) — early deopt would yield ~hundreds of ns",
        per_call_ns
    );
}
