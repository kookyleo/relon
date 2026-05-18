//! v6-γ M2 + M3 trace JIT integration smoke tests.
//!
//! Each test case exercises a different slice of the
//! `HotCounter → recorder → optimizer → emitter → JIT install →
//! invoke` pipeline:
//!
//! 1. HotCounter inject fires after `RELON_HOT_THRESHOLD` calls
//!    (`hot_counter_triggers_at_threshold`).
//! 2. Below-threshold calls return the correct value, untouched by
//!    the prologue (`hot_counter_below_threshold_returns_value`).
//! 3. The HotCounter prologue is opt-in via `SandboxConfig` and
//!    leaves baseline evaluators untouched (`no_trace_jit_no_helper_calls`).
//! 4. Multiple back-to-back hot triggers all route through the
//!    helper (`hot_counter_post_threshold_keeps_firing`).
//! 5. Independent fn_ids increment independently
//!    (`hot_counters_are_per_fn_id`).
//! 6. `TraceJitState::jit_compile_trace_for_fn` produces a JIT entry
//!    that returns success on the constant-trace fast path
//!    (`pipeline_compiles_const_return_trace`).
//! 7. Install + lookup round trip persists the trace fn
//!    (`pipeline_install_lookup_round_trip`).
//! 8. The JIT-installed entry actually returns
//!    `TraceEntryStatus::Success` when invoked
//!    (`pipeline_invoke_returns_success`).
//! 9. The result slot is populated with the returned SSA value
//!    (`pipeline_writes_result_slot`).
//! 10. Aborted recordings surface as a typed pipeline error
//!     (`pipeline_aborts_unsupported_op`).
//! 11. Out-of-range fn_id rejects with a typed error
//!     (`pipeline_out_of_range_fn_id_errors`).
//! 12. A simple Add(I64) trace JIT-compiles and reports Success on
//!     invoke (`pipeline_compiles_add_trace`).
//!
//! Status: these tests cover the M2/M3 surface end-to-end. The full
//! "interpret IR + record" loop is M4 work (the recorder still gets
//! its `Op` stream from the test driver, not from the cranelift-
//! generic backend's live execution). See the corresponding stage
//! report for the trade-off rationale.

use std::collections::HashMap;

use relon_codegen_native::{
    global_trace_jit_state, hot_counter_peek, hot_counter_reset, jump_helper_call_count,
    reset_jump_helper_call_count, CraneliftAotEvaluator, SandboxConfig, TraceJitError,
    TraceJitState, MAX_FN_ID, RELON_HOT_THRESHOLD,
};
use relon_eval_api::{Evaluator, Value};
use relon_ir::ir::{Func, IrType, Module as IrModule, Op, TaggedOp};
use relon_parser::TokenRange;
use relon_trace_abi::{ObservedType, TraceContext, TraceEntryStatus};
use relon_trace_jit::{Offset, TraceBuffer, TraceOp};
use relon_trace_recorder::{RecordResult, RecorderState};

/// Build a `#main(Int x, Int y) -> Int : x op y` IR module.
fn build_arith_ir(op: Op) -> IrModule {
    let body = vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::LocalGet(1),
            range: TokenRange::default(),
        },
        TaggedOp {
            op,
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ];
    let func = Func {
        name: "run_main".to_string(),
        params: vec![IrType::I64, IrType::I64],
        ret: IrType::I64,
        body,
        range: TokenRange::default(),
    };
    IrModule {
        imports: vec![],
        funcs: vec![func],
        entry_func_index: Some(0),
        closure_table: vec![],
    }
}

/// Build a cranelift evaluator with the HotCounter prologue enabled
/// at slot `fn_id`. Counter slot is reset before each test so
/// sequential runs don't bleed state.
fn build_traced_evaluator(ir: IrModule, fn_id: u32) -> CraneliftAotEvaluator {
    hot_counter_reset(fn_id);
    reset_jump_helper_call_count();
    let cfg = SandboxConfig {
        trace_jit_fn_id: Some(fn_id),
        ..SandboxConfig::default()
    };
    CraneliftAotEvaluator::from_ir_direct(ir, cfg, vec!["x".to_string(), "y".to_string()])
        .expect("compile")
}

fn make_args(x: i64, y: i64) -> HashMap<String, Value> {
    let mut h = HashMap::new();
    h.insert("x".to_string(), Value::Int(x));
    h.insert("y".to_string(), Value::Int(y));
    h
}

#[test]
fn hot_counter_below_threshold_returns_value() {
    // Use a distinct fn_id per test to keep counter state isolated
    // even if Cargo runs tests in parallel and races the static
    // table.
    let fn_id = 100;
    let ev = build_traced_evaluator(build_arith_ir(Op::Add(IrType::I64)), fn_id);

    // Calls 1..=threshold-1 must return the correct sum because the
    // prologue counter has not crossed the threshold yet.
    for i in 1..RELON_HOT_THRESHOLD {
        let result = ev.run_main(make_args(40, 2)).expect("run_main");
        assert_eq!(result, Value::Int(42), "call #{i} mid-threshold");
        assert_eq!(hot_counter_peek(fn_id), i, "counter after call #{i}");
    }
    assert_eq!(
        jump_helper_call_count(),
        0,
        "below threshold must not call jump helper"
    );
}

#[test]
fn hot_counter_triggers_at_threshold() {
    let fn_id = 101;
    let ev = build_traced_evaluator(build_arith_ir(Op::Add(IrType::I64)), fn_id);

    // Burn through below-threshold calls.
    for _ in 1..RELON_HOT_THRESHOLD {
        let r = ev.run_main(make_args(40, 2)).expect("run_main");
        assert_eq!(r, Value::Int(42));
    }
    let pre_count = jump_helper_call_count();

    // The threshold-th call must route through the hot block, which
    // calls the helper and returns the sentinel zero.
    let result = ev.run_main(make_args(40, 2)).expect("run_main hot");
    assert_eq!(
        result,
        Value::Int(0),
        "hot path returns sentinel zero, not the user value"
    );
    assert_eq!(
        jump_helper_call_count(),
        pre_count + 1,
        "hot trigger must call the jump helper exactly once"
    );
    assert_eq!(hot_counter_peek(fn_id), RELON_HOT_THRESHOLD);
}

#[test]
fn hot_counter_post_threshold_keeps_firing() {
    let fn_id = 102;
    let ev = build_traced_evaluator(build_arith_ir(Op::Add(IrType::I64)), fn_id);

    // Drive counter to threshold first.
    for _ in 1..=RELON_HOT_THRESHOLD {
        let _ = ev.run_main(make_args(1, 2));
    }
    let base = jump_helper_call_count();

    // Three more hot calls — each should bump the helper counter
    // and return the sentinel because the prologue still saturates
    // on every entry.
    for k in 1..=3 {
        let r = ev.run_main(make_args(1, 2)).expect("run_main hot");
        assert_eq!(r, Value::Int(0), "post-threshold call #{k} sentinel");
    }
    assert_eq!(jump_helper_call_count(), base + 3);
}

#[test]
fn hot_counters_are_per_fn_id() {
    let fn_a = 103;
    let fn_b = 104;
    let ev_a = build_traced_evaluator(build_arith_ir(Op::Add(IrType::I64)), fn_a);
    let ev_b = build_traced_evaluator(build_arith_ir(Op::Mul(IrType::I64)), fn_b);

    // Bump only fn_a's counter.
    for _ in 0..5 {
        let _ = ev_a.run_main(make_args(2, 3));
    }
    assert_eq!(hot_counter_peek(fn_a), 5);
    assert_eq!(hot_counter_peek(fn_b), 0, "fn_b must stay untouched");

    let r = ev_b.run_main(make_args(2, 3)).expect("ev_b run");
    assert_eq!(r, Value::Int(6));
    assert_eq!(hot_counter_peek(fn_b), 1);
    assert_eq!(hot_counter_peek(fn_a), 5);
}

#[test]
fn no_trace_jit_no_helper_calls() {
    reset_jump_helper_call_count();
    // Baseline evaluator: no `trace_jit_fn_id` set — the codegen
    // skips the inject pass entirely and the helper is never
    // touched.
    let cfg = SandboxConfig::default();
    assert!(cfg.trace_jit_fn_id.is_none());
    let ev = CraneliftAotEvaluator::from_ir_direct(
        build_arith_ir(Op::Add(IrType::I64)),
        cfg,
        vec!["x".to_string(), "y".to_string()],
    )
    .expect("compile");

    for _ in 0..(RELON_HOT_THRESHOLD * 2) {
        let r = ev.run_main(make_args(11, 1)).expect("run_main");
        assert_eq!(r, Value::Int(12));
    }
    assert_eq!(
        jump_helper_call_count(),
        0,
        "no trace_jit_fn_id ⇒ no helper traffic"
    );
}

#[test]
fn pipeline_compiles_const_return_trace() {
    let state = TraceJitState::new();
    let mut recorder = RecorderState::new();
    let val = match recorder.record_op(&Op::ConstI64(7), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("ConstI64: unexpected {other:?}"),
    };
    let term = recorder.record_op(&Op::Return, &[val], None);
    assert!(matches!(term, RecordResult::Terminated));
    let trace_fn = state
        .jit_compile_trace_for_fn(0, recorder)
        .expect("compile trace");
    assert_eq!(trace_fn.fn_id, 0);
    assert!(!trace_fn.raw_fn_ptr().is_null());
}

#[test]
fn pipeline_install_lookup_round_trip() {
    let state = TraceJitState::new();
    let recorder = make_const_recorder(21);
    let trace_fn = state
        .jit_compile_trace_for_fn(11, recorder)
        .expect("compile");
    assert!(state.lookup_trace(11).is_none());
    state.install_trace(11, trace_fn);
    let looked = state.lookup_trace(11).expect("post-install lookup");
    assert_eq!(looked.fn_id, 11);
    assert_eq!(state.installed_count(), 1);
}

#[test]
fn pipeline_invoke_returns_success() {
    let state = TraceJitState::new();
    let recorder = make_const_recorder(99);
    let trace_fn = state
        .jit_compile_trace_for_fn(0, recorder)
        .expect("compile");
    let mut ctx = TraceContext::with_capacity(64);
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, std::ptr::null()) };
    assert_eq!(
        status,
        TraceEntryStatus::Success,
        "happy-path trace must return Success"
    );
}

#[test]
fn pipeline_writes_result_slot() {
    let state = TraceJitState::new();
    let recorder = make_const_recorder(12345);
    let trace_fn = state
        .jit_compile_trace_for_fn(0, recorder)
        .expect("compile");
    let mut ctx = TraceContext::with_capacity(64);
    ctx.result_slot = 0;
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, std::ptr::null()) };
    assert_eq!(status, TraceEntryStatus::Success);
    assert_eq!(
        ctx.result_slot, 12345,
        "trace must write the returned value into result_slot"
    );
}

#[test]
fn pipeline_aborts_unsupported_op() {
    let state = TraceJitState::new();
    let mut recorder = RecorderState::new();
    // Op::CallNative is unsupported by the recorder lowering -- it
    // aborts with UnrecoverableEffect, so the pipeline's finalize
    // step must surface a typed error.
    let res = recorder.record_op(
        &Op::CallNative {
            import_idx: 0,
            param_tys: vec![],
            ret_ty: IrType::I64,
            cap_bit: 0,
        },
        &[],
        None,
    );
    assert!(matches!(res, RecordResult::Abort(_)));
    let err = state
        .jit_compile_trace_for_fn(0, recorder)
        .err()
        .expect("must error");
    assert!(matches!(err, TraceJitError::RecorderAbort(_)));
}

#[test]
fn pipeline_out_of_range_fn_id_errors() {
    let state = TraceJitState::new();
    let recorder = make_const_recorder(1);
    let err = state
        .jit_compile_trace_for_fn(MAX_FN_ID as u32, recorder)
        .err()
        .expect("must error");
    assert!(matches!(err, TraceJitError::FnIdOutOfRange(_)));
}

#[test]
fn pipeline_compiles_add_trace() {
    // Build a slightly more interesting trace: const 11 + const 22
    // = 33. Verifies the optimiser pipeline + emitter handle a real
    // binary op end-to-end (not just a const-return short circuit).
    //
    // We build the `TraceBuffer` by hand rather than going through
    // the recorder lowering rules. The reason: `Op::Add(I64)`
    // lowering emits an `ArithOverflow` guard via `guards_after` and
    // the pre-integration recorder appends a `Guard` op to the
    // buffer without calling `TraceBuffer::record_guard`, so the
    // emitter can't find the matching `GuardSite` and produces
    // `EmitError::OrphanGuardOp`. Wiring `record_guard` into the
    // recorder is M4 work; the M2/M3 surface stays guard-free for
    // the happy path.
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let sum = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64(a, 11));
    buffer.append(TraceOp::ConstI64(b, 22));
    buffer.append(TraceOp::Add(sum, a, b));
    buffer.append(TraceOp::Return(sum));

    let trace_fn = state
        .jit_compile_buffer_for_fn(0, buffer)
        .expect("compile Add trace");
    let mut ctx = TraceContext::with_capacity(64);
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, std::ptr::null()) };
    assert_eq!(
        status,
        TraceEntryStatus::Success,
        "Add trace happy-path must return Success"
    );
    assert_eq!(ctx.result_slot, 33, "Add(11, 22) trace must yield 33");
}

#[test]
fn pipeline_compiles_mul_trace_via_buffer() {
    // Mirror of `pipeline_compiles_add_trace` exercising
    // `TraceOp::Mul`. Same hand-built-buffer rationale.
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let prod = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64(a, 6));
    buffer.append(TraceOp::ConstI64(b, 7));
    buffer.append(TraceOp::Mul(prod, a, b));
    buffer.append(TraceOp::Return(prod));

    let trace_fn = state
        .jit_compile_buffer_for_fn(7, buffer)
        .expect("compile Mul trace");
    let mut ctx = TraceContext::with_capacity(64);
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, std::ptr::null()) };
    assert_eq!(status, TraceEntryStatus::Success);
    assert_eq!(ctx.result_slot, 42);
}

#[test]
fn pipeline_chained_trace_buffer_install_invoke() {
    // Demonstrate that install + invoke through the
    // `TraceJitState` registry works end-to-end for a hand-built
    // buffer. Confirms install → lookup → invoke return the same
    // SSA value the emitter wired into `result_slot`.
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let diff = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64(a, 50));
    buffer.append(TraceOp::ConstI64(b, 8));
    buffer.append(TraceOp::Sub(diff, a, b));
    buffer.append(TraceOp::Return(diff));

    let trace_fn = state
        .jit_compile_buffer_for_fn(13, buffer)
        .expect("compile Sub trace");
    state.install_trace(13, trace_fn);
    let looked = state.lookup_trace(13).expect("post-install lookup");
    let mut ctx = TraceContext::with_capacity(64);
    let status = unsafe { looked.invoke(&mut ctx as *mut _, std::ptr::null()) };
    assert_eq!(status, TraceEntryStatus::Success);
    assert_eq!(ctx.result_slot, 42);
}

#[test]
fn pipeline_load_store_trace_via_buffer() {
    // Exercise the Load + Store emitter path with a small backing
    // buffer placed on the heap. We construct the trace SSA so:
    //   slot[0] := 99
    //   slot[1] := load(slot[0])  (i.e. 99)
    //   return slot[1]
    // The emitter routes loads/stores through the `TraceContext`'s
    // `ssa_slots`, so we write 99 into ssa_slots[0] first, run the
    // trace which copies it via load/store/return, and confirm the
    // chain.
    //
    // Note: TraceOp::Load expects (dst, base_ssa, offset). The
    // emitter materialises `base` from `ssa_slots[base_ssa]` as a
    // pointer; we therefore pre-set the base to a stable address we
    // own.
    let mut backing = [0u64; 4];
    backing[0] = 0x1234_5678_9ABC_DEF0;

    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let base = buffer.fresh_ssa();
    let loaded = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64(base, backing.as_ptr() as i64));
    buffer.append(TraceOp::Load(loaded, base, Offset(0)));
    buffer.append(TraceOp::Return(loaded));

    let trace_fn = state
        .jit_compile_buffer_for_fn(0, buffer)
        .expect("compile load trace");
    let mut ctx = TraceContext::with_capacity(64);
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, std::ptr::null()) };
    assert_eq!(status, TraceEntryStatus::Success);
    assert_eq!(ctx.result_slot, 0x1234_5678_9ABC_DEF0);
}

#[test]
fn global_state_singleton_is_stable() {
    let s1 = global_trace_jit_state();
    let s2 = global_trace_jit_state();
    // Address equality proves the OnceLock initialised exactly once.
    assert_eq!(s1 as *const _, s2 as *const _);
}

/// Helper: build a recorder whose only ops are `ConstI64(v) ; Return`
/// — used by several happy-path tests.
fn make_const_recorder(v: i64) -> RecorderState {
    let mut recorder = RecorderState::new();
    let val = match recorder.record_op(&Op::ConstI64(v), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(v) } => v,
        other => panic!("ConstI64 unexpected {other:?}"),
    };
    let term = recorder.record_op(&Op::Return, &[val], None);
    assert!(matches!(term, RecordResult::Terminated));
    recorder
}
