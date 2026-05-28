//! W5 layout-variance RCA probe (2026-05-26).
//!
//! Drives the W5 recorder body through the install pipeline once, then
//! runs the installed trace in tight 50_000-iter batches multiple
//! times in a single process. For each batch we print:
//!
//! * `addr_trace_fn`: trace entry pointer (cranelift JIT module mmap)
//! * `addr_tctx`: stack address of the per-batch `TraceContext`
//! * `addr_dict`: heap address of the dict-record `Vec` payload
//! * `addr_keys`: heap address of the keys-list `Vec` payload
//! * `addr_ic`: address of the dict-lookup IC table inside `tctx`
//! * `addr_ssa`: address of `tctx.ssa_slots[0]`
//! * `ns_per_call`: wall time per `invoke_with_existing_ctx`
//!
//! Set `RELON_TRACE_FN_ALIGN_LOG2=N` (e.g. `6`) at process launch to
//! force the cranelift trace fn alignment. With `0` (default) only
//! page-aligned addresses are observed because each fresh JIT module
//! allocates one code page.
//!
//! Optional knobs:
//! * `W5_PROBE_BATCHES=20` — number of timing batches (default 16)
//! * `W5_PROBE_ITERS=50000` — iters per batch (default 10000)
//! * `W5_PROBE_N=10000` — n parameter for the body (default 10000)

use std::sync::Arc;
use std::time::Instant;

use relon_codegen_cranelift::trace_install::{
    __relon_jump_to_recorder, clear_recording, default_host_hooks, global_trace_jit_state,
    register_recording, JITedTraceFn, RecordingRegistration,
};
use relon_codegen_cranelift::{RecordingOutcome, TraceRecordingEvaluator};
use relon_ir::ir::{IrType, Op, TaggedOp};
use relon_ir::shape_hash::shape_hash_for_keys;
use relon_parser::TokenRange;
use relon_trace_abi::TraceContext;
use relon_trace_jit::{build_dict_record_v2, build_flat_list_record, build_string_record};
use relon_trace_recorder::RecorderState;

fn tag(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

fn build_body(shape_hash: u64, record_len: u32) -> Vec<TaggedOp> {
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
                        entry_count_hint: None,
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

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn install_w5_trace(fn_id: u32, n: u64) -> (Arc<JITedTraceFn>, Vec<u8>, Vec<u8>) {
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

    let body = build_body(shape_hash, dict_bytes.len() as u32);
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    state.invalidate_trace(fn_id);

    // Pre-flight dry-run: surface any recorder rejection up front
    // rather than the (cryptic) install lookup miss after the
    // recorder route returns silently.
    {
        let param_tys = [IrType::I64, IrType::I64, IrType::I64];
        let args_inputs: Vec<(u64, IrType)> = param_tys
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                (
                    match i {
                        0 => n,
                        1 => dict_bytes.as_ptr() as u64,
                        2 => keys_list_bytes.as_ptr() as u64,
                        _ => unreachable!(),
                    },
                    *ty,
                )
            })
            .collect();
        let mut recorder = RecorderState::new();
        let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &args_inputs, &body);
        if let RecordingOutcome::Aborted { reason, .. } = outcome {
            panic!("W5 recorder dry-run aborted: {reason:?}");
        }
    }

    register_recording(
        fn_id,
        RecordingRegistration {
            body,
            param_tys: vec![IrType::I64, IrType::I64, IrType::I64],
            ..Default::default()
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
    let trace_fn = state
        .lookup_trace(fn_id)
        .expect("W5 trace install must succeed");

    // Keep `_key_records` alive for the lifetime of the function by
    // leaking them — the fixture pointers are baked into
    // `keys_list_bytes` and dereferenced on every iter.
    Box::leak(Box::new(key_records));

    (trace_fn, dict_bytes, keys_list_bytes)
}

fn main() {
    let n = env_usize("W5_PROBE_N", 10_000) as u64;
    let batches = env_usize("W5_PROBE_BATCHES", 16);
    let iters = env_usize("W5_PROBE_ITERS", 10_000);

    // fn_id must be < MAX_FN_ID (1024).
    // Pre-install N dummy W5-shaped traces to mimic the criterion
    // bench process where 9-10 traces are co-resident in the JIT
    // address space (fn_ids 220/221 + 1007-1014). The bench's
    // `bench_function` setup runs ALL group init paths even when the
    // user filters via `--bench W5_only`, so a real bench process
    // always pays this co-install cost.
    let n_co_traces = env_usize("W5_PROBE_CO_TRACES", 0);
    for i in 0..n_co_traces {
        let id = 100 + (i as u32);
        let _ = install_w5_trace(id, n);
    }

    let fn_id: u32 = 555;
    let (_trace_fn, dict_bytes, keys_list_bytes) = install_w5_trace(fn_id, n);
    let trace_fn = global_trace_jit_state().lookup_trace(fn_id).unwrap();
    let fn_ptr_addr = trace_fn.raw_fn_ptr() as usize;

    let args: [u64; 3] = [
        n,
        dict_bytes.as_ptr() as u64,
        keys_list_bytes.as_ptr() as u64,
    ];
    let expected: u64 = (n / 10) * 55;

    let dict_addr = dict_bytes.as_ptr() as usize;
    let keys_addr = keys_list_bytes.as_ptr() as usize;

    println!(
        "# W5 variance probe — n={n} batches={batches} iters_per_batch={iters} expected_sum={expected}"
    );
    println!(
        "# addr_trace_fn=0x{:016x} mod16={} mod32={} mod64={} mod128={} mod4096={}",
        fn_ptr_addr,
        fn_ptr_addr % 16,
        fn_ptr_addr % 32,
        fn_ptr_addr % 64,
        fn_ptr_addr % 128,
        fn_ptr_addr % 4096
    );
    println!(
        "# addr_dict    =0x{:016x} mod64={} mod4096={}",
        dict_addr,
        dict_addr % 64,
        dict_addr % 4096
    );
    println!(
        "# addr_keys    =0x{:016x} mod64={} mod4096={}",
        keys_addr,
        keys_addr % 64,
        keys_addr % 4096
    );
    println!(
        "# columns: batch addr_tctx mod64_tctx addr_ic mod64_ic addr_ssa mod64_ssa ns_per_call"
    );

    let state = global_trace_jit_state();

    // Warmup batch (excluded from stats): primes L1d/L2 with the dict
    // bytes + warms the IC. Uses a separate tctx to mirror the bench's
    // pre-warmup invoke.
    {
        let mut tctx = TraceContext::with_hooks(64, default_host_hooks());
        for _ in 0..1000 {
            let _ = unsafe {
                state.invoke_with_existing_ctx(fn_id, &mut tctx, args.as_ptr(), |_| expected)
            };
        }
    }

    let mut sums = Vec::with_capacity(batches);
    for b in 0..batches {
        // Fresh tctx per batch — mirrors what criterion's iter_custom
        // does at each sample boundary.
        let mut tctx = TraceContext::with_hooks(64, default_host_hooks());
        let tctx_addr = &tctx as *const _ as usize;
        let ic_addr = tctx.dict_lookup_ic.as_ptr() as usize;
        let ssa_addr = tctx.ssa_slots.as_ptr() as usize;

        // In-batch warmup: 200 iters to settle branch predictor +
        // re-hydrate IC with the post-allocation cold cache lines.
        for _ in 0..200 {
            let _ = unsafe {
                state.invoke_with_existing_ctx(fn_id, &mut tctx, args.as_ptr(), |_| expected)
            };
        }

        let t0 = Instant::now();
        for _ in 0..iters {
            let v = unsafe {
                state.invoke_with_existing_ctx(fn_id, &mut tctx, args.as_ptr(), |_| expected)
            };
            std::hint::black_box(v);
        }
        let elapsed = t0.elapsed();
        let ns_per_call = elapsed.as_nanos() / iters as u128;
        sums.push(ns_per_call);
        println!(
            "{b:3} 0x{tctx_addr:016x} {:3} 0x{ic_addr:016x} {:3} 0x{ssa_addr:016x} {:3} {ns_per_call}",
            tctx_addr % 64,
            ic_addr % 64,
            ssa_addr % 64,
        );
    }

    sums.sort_unstable();
    let median = sums[sums.len() / 2];
    let min = sums.first().copied().unwrap_or(0);
    let max = sums.last().copied().unwrap_or(0);
    let span = if min > 0 {
        max as f64 / min as f64
    } else {
        0.0
    };
    eprintln!("# stats: min={min}ns median={median}ns max={max}ns max/min={span:.2}x");
}
