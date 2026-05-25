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
    reset_jump_helper_call_count, AotEvaluator, SandboxConfig, TraceJitError,
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
fn build_traced_evaluator(ir: IrModule, fn_id: u32) -> AotEvaluator {
    hot_counter_reset(fn_id);
    reset_jump_helper_call_count();
    let cfg = SandboxConfig {
        trace_jit_fn_id: Some(fn_id),
        ..SandboxConfig::default()
    };
    AotEvaluator::from_ir_direct(ir, cfg, vec!["x".to_string(), "y".to_string()])
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
    let ev = AotEvaluator::from_ir_direct(
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
    buffer.append(TraceOp::ConstI64 { dst: a, value: 11 });
    buffer.append(TraceOp::ConstI64 { dst: b, value: 22 });
    buffer.append(TraceOp::Add {
        dst: sum,
        lhs: a,
        rhs: b,
    });
    buffer.append(TraceOp::Return { value: sum });

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
    buffer.append(TraceOp::ConstI64 { dst: a, value: 6 });
    buffer.append(TraceOp::ConstI64 { dst: b, value: 7 });
    buffer.append(TraceOp::Mul {
        dst: prod,
        lhs: a,
        rhs: b,
    });
    buffer.append(TraceOp::Return { value: prod });

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
    buffer.append(TraceOp::ConstI64 { dst: a, value: 50 });
    buffer.append(TraceOp::ConstI64 { dst: b, value: 8 });
    buffer.append(TraceOp::Sub {
        dst: diff,
        lhs: a,
        rhs: b,
    });
    buffer.append(TraceOp::Return { value: diff });

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
    buffer.append(TraceOp::ConstI64 {
        dst: base,
        value: backing.as_ptr() as i64,
    });
    buffer.append(TraceOp::Load {
        dst: loaded,
        base,
        offset: Offset(0),
    });
    buffer.append(TraceOp::Return { value: loaded });

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

// ---- v6-γ M4: real `__relon_jump_to_recorder` driver tests ----

use relon_codegen_native::{
    clear_recording, recording_registration_count, register_recording, RecordingRegistration,
};

#[test]
fn jump_helper_no_registration_is_noop() {
    // fn_id not in the recording registry → helper logs + returns
    // without installing anything. We can't observe the log without
    // a subscriber, but no trace for this fn_id must appear in the
    // global registry. Note: we deliberately scope the assertion to
    // this fn_id (rather than the global `installed_count()`) so the
    // test stays robust against parallel-test pollution — other
    // tests in this binary share `global_trace_jit_state()` and
    // install / invalidate their own fn_ids concurrently, which
    // would flake a count-delta check.
    let state = global_trace_jit_state();
    // Pick a fn_id that no other test uses.
    let fn_id = 700u32;
    let _ = clear_recording(fn_id);
    let _ = state.invalidate_trace(fn_id);
    reset_jump_helper_call_count();
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, std::ptr::null());
    }
    assert_eq!(jump_helper_call_count(), 1);
    assert!(
        state.lookup_trace(fn_id).is_none(),
        "no registration → no trace installed for this fn_id"
    );
}

#[test]
fn jump_helper_installs_const_trace_from_registry() {
    // Register a (#main() -> Int : 99) body. The helper should walk
    // it via TraceRecordingEvaluator, drive the install pipeline,
    // and end up with a trace installed for the fn_id. Subsequent
    // invocations of the helper short-circuit.
    let fn_id = 701u32;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    // Bench-style: install_trace doesn't expose an uninstall API
    // today; skip the test if a previous run left state behind.
    if state.lookup_trace(fn_id).is_some() {
        // Best-effort cleanup is unavailable; skip rather than fail.
        return;
    }

    register_recording(
        fn_id,
        RecordingRegistration {
            body: vec![
                TaggedOp {
                    op: Op::ConstI64(99),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            param_tys: vec![],
        },
    );
    reset_jump_helper_call_count();
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, std::ptr::null());
    }
    assert_eq!(jump_helper_call_count(), 1);
    let installed = state.lookup_trace(fn_id).expect("trace installed");
    let mut ctx = TraceContext::with_capacity(64);
    let status = unsafe { installed.invoke(&mut ctx as *mut _, std::ptr::null()) };
    assert_eq!(status, TraceEntryStatus::Success);
    assert_eq!(ctx.result_slot, 99);

    // A second helper invocation must short-circuit (trace already
    // installed); the diagnostic counter still bumps because the
    // entry path is unchanged. Capture the per-fn_id `Arc` before
    // and after, so the assertion is robust against parallel tests
    // mutating other entries in `global_trace_jit_state()` between
    // the two reads.
    use std::sync::Arc;
    let pre_trace = state.lookup_trace(fn_id).expect("pre: trace installed");
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, std::ptr::null());
    }
    assert_eq!(jump_helper_call_count(), 2);
    let post_trace = state
        .lookup_trace(fn_id)
        .expect("post: trace still installed");
    assert!(
        Arc::ptr_eq(&pre_trace, &post_trace),
        "second hot trigger must not replace the installed trace"
    );

    let _ = clear_recording(fn_id);
}

#[test]
fn recording_registry_round_trip() {
    let fn_id = 702u32;
    let pre = recording_registration_count();
    let prev = register_recording(
        fn_id,
        RecordingRegistration {
            body: vec![],
            param_tys: vec![IrType::I64],
        },
    );
    assert!(prev.is_none(), "fresh fn_id had no prior registration");
    assert_eq!(recording_registration_count(), pre + 1);
    let removed = clear_recording(fn_id).expect("must have been registered");
    assert_eq!(removed.param_tys, vec![IrType::I64]);
    assert_eq!(recording_registration_count(), pre);
}

#[test]
fn invoke_with_fallback_returns_trace_result_on_success() {
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    // Trace body must clear `TINY_TRACE_OP_THRESHOLD` so the
    // dispatcher actually invokes the trace rather than gating it
    // straight to the fallback. A chained `Add` sequence over a
    // const seed produces an op_count >= 5 while keeping the trace
    // semantically equivalent to "return 0x77".
    let zero = buffer.fresh_ssa();
    let one = buffer.fresh_ssa();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let c = buffer.fresh_ssa();
    let d = buffer.fresh_ssa();
    let e = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64 {
        dst: zero,
        value: 0x77 - 5,
    });
    buffer.append(TraceOp::ConstI64 { dst: one, value: 1 });
    buffer.append(TraceOp::Add {
        dst: a,
        lhs: zero,
        rhs: one,
    });
    buffer.append(TraceOp::Add {
        dst: b,
        lhs: a,
        rhs: one,
    });
    buffer.append(TraceOp::Add {
        dst: c,
        lhs: b,
        rhs: one,
    });
    buffer.append(TraceOp::Add {
        dst: d,
        lhs: c,
        rhs: one,
    });
    buffer.append(TraceOp::Add {
        dst: e,
        lhs: d,
        rhs: one,
    });
    buffer.append(TraceOp::Return { value: e });
    let trace_fn = state
        .jit_compile_buffer_for_fn(801, buffer)
        .expect("compile");
    state.install_trace(801, trace_fn);

    let fallback_called = std::cell::Cell::new(false);
    let result = unsafe {
        state.invoke_with_fallback(801, std::ptr::null(), 32, |_| {
            fallback_called.set(true);
            0
        })
    };
    assert_eq!(result, 0x77);
    assert!(!fallback_called.get(), "trace Success must skip fallback");
}

/// `TINY_TRACE_OP_THRESHOLD` gate: micro-traces whose body is
/// below the threshold MUST route directly to the fallback closure
/// rather than incurring the trace-entry prologue. Guards W12's
/// 2.08× regression fix; see `TINY_TRACE_OP_THRESHOLD` docs.
#[test]
fn invoke_with_fallback_gates_micro_traces_to_fallback() {
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let v = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64 {
        dst: v,
        value: 0xdead,
    });
    buffer.append(TraceOp::Return { value: v });
    let trace_fn = state
        .jit_compile_buffer_for_fn(803, buffer)
        .expect("compile");
    state.install_trace(803, trace_fn);

    let fallback_called = std::cell::Cell::new(false);
    let result = unsafe {
        state.invoke_with_fallback(803, std::ptr::null(), 32, |_| {
            fallback_called.set(true);
            0xbeef
        })
    };
    assert_eq!(
        result, 0xbeef,
        "micro-trace must route to fallback, not the trace body"
    );
    assert!(
        fallback_called.get(),
        "fallback closure must have been invoked"
    );
}

#[test]
fn invoke_with_fallback_runs_fallback_when_no_trace() {
    let state = TraceJitState::new();
    let fallback_called = std::cell::Cell::new(false);
    let result = unsafe {
        state.invoke_with_fallback(802, std::ptr::null(), 32, |_| {
            fallback_called.set(true);
            0x123
        })
    };
    assert_eq!(result, 0x123);
    assert!(fallback_called.get());
}

#[test]
fn invalidate_trace_drops_the_install() {
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let v = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64 { dst: v, value: 1 });
    buffer.append(TraceOp::Return { value: v });
    let trace_fn = state
        .jit_compile_buffer_for_fn(803, buffer)
        .expect("compile");
    state.install_trace(803, trace_fn);
    assert!(state.lookup_trace(803).is_some());
    let dropped = state.invalidate_trace(803);
    assert!(dropped.is_some());
    assert!(state.lookup_trace(803).is_none());
}

#[test]
fn default_host_hooks_populates_save_deopt_slot() {
    use relon_codegen_native::default_host_hooks;
    let hooks = default_host_hooks();
    assert!(hooks.save_deopt.is_some(), "save_deopt must be wired");
    let mut ctx = TraceContext::with_hooks(8, hooks);
    assert!(ctx.host_hooks.save_deopt.is_some());
    // Confirm we can call through the table without crashing the
    // host. The shim records a snapshot into ctx.deopt_state.
    let f = ctx.host_hooks.save_deopt.expect("populated");
    // v6-δ M1 R5: save_deopt slot now carries the full
    // (ctx, guard_pc, external_pc) signature.
    unsafe { f(&mut ctx as *mut _, 42, 0xfeedbeef) };
    let snap = ctx.deopt_state.as_ref().expect("snapshot populated");
    assert_eq!(snap.guard_pc, 42);
}

/// v6-δ M1 R1: a recording driver that hits `Op::LocalGet` followed
/// by an arith op must install successfully. The recorder now emits
/// `TraceOp::LocalGet { dst, slot_idx }` on first observation, and the
/// emitter materialises the SSA from the entry-fn's `args_ptr` so
/// `Add` no longer surfaces `EmitError::UnboundSsa`.
#[test]
fn let_with_arg_use_installs_via_local_get_lowering() {
    let fn_id = 720u32;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body: vec![
                TaggedOp {
                    op: Op::LocalGet(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::LocalGet(1),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Add(IrType::I64),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            // The recorder seeds `LocalGet` with `ObservedType::I32`;
            // declare the params accordingly so the TypeCheck guard
            // policy does not flip the recording into an abort.
            param_tys: vec![IrType::I32, IrType::I32],
        },
    );
    // The trace body reads two args; pass non-overflowing values
    // so the `ArithOverflow` guard does not deopt.
    let raw_args: [u64; 2] = [100u64, 23u64];
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, raw_args.as_ptr());
    }
    assert!(
        state.lookup_trace(fn_id).is_some(),
        "LocalGet + Add must install (R1)"
    );

    // Invoke the trace; the JIT body reads args_ptr[0] + args_ptr[1]
    // and should return 123.
    let mut ctx = TraceContext::with_capacity(32);
    let trace_fn = state.lookup_trace(fn_id).expect("post-install");
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, raw_args.as_ptr()) };
    assert!(
        matches!(status, TraceEntryStatus::Success),
        "expected Success, got {status:?}"
    );
    assert_eq!(
        ctx.result_slot, 123,
        "LocalGet(0) + LocalGet(1) must materialise the packed args"
    );

    let _ = clear_recording(fn_id);
    let _ = state.invalidate_trace(fn_id);
}

/// v6-δ M1 R2: with `sadd_overflow` lowering wired through the
/// emitter, the trace's `ArithOverflow` guard reads a real carry bit
/// instead of a constant-0 predicate. Non-overflowing inputs run the
/// hot path to completion; overflowing inputs deopt cleanly to the
/// fallback closure.
#[test]
fn arith_overflow_guard_uses_real_carry_bit() {
    let fn_id = 721u32;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body: vec![
                TaggedOp {
                    op: Op::LocalGet(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::LocalGet(1),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Add(IrType::I64),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            param_tys: vec![IrType::I32, IrType::I32],
        },
    );
    // Warm up with non-overflowing values.
    let warm_args: [u64; 2] = [1u64, 2u64];
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, warm_args.as_ptr());
    }
    assert!(state.lookup_trace(fn_id).is_some(), "trace must install");

    // 1) Non-overflowing happy path: 1 + 2 = 3 (same args as warmup).
    let trace_fn = state.lookup_trace(fn_id).expect("trace");
    let mut ctx_ok = TraceContext::with_capacity(32);
    let status_ok = unsafe { trace_fn.invoke(&mut ctx_ok as *mut _, warm_args.as_ptr()) };
    assert!(
        matches!(status_ok, TraceEntryStatus::Success),
        "expected Success for non-overflow, got {status_ok:?}"
    );
    assert_eq!(ctx_ok.result_slot, 3);

    // 2) Overflow: i64::MAX + 1 must deopt. Expect GuardFailed. The
    //    deopt block must call `__relon_trace_save_deopt` through the
    //    proper FuncId-based import — v6-δ M1 fix; the historical
    //    UserExternalName(0, 0) layout would have called back into
    //    the trace fn itself.
    let of_args: [u64; 2] = [i64::MAX as u64, 1u64];
    let mut ctx_of = TraceContext::with_capacity(32);
    let status_of = unsafe { trace_fn.invoke(&mut ctx_of as *mut _, of_args.as_ptr()) };
    assert!(
        matches!(status_of, TraceEntryStatus::GuardFailed),
        "expected GuardFailed for overflow, got {status_of:?}"
    );
    assert!(
        ctx_of.deopt_state.is_some(),
        "GuardFailed must populate deopt_state via save_deopt"
    );

    let _ = clear_recording(fn_id);
    let _ = state.invalidate_trace(fn_id);
}

/// v6-δ M1 R3: `invoke_with_resume` surfaces the full
/// `DeoptStateSnapshot` to the fallback closure so callers can feed
/// `local_slots` into `Evaluator::resume_from_pc` for partial-resume.
/// On GuardFailed we observe the snapshot's `external_pc` + populated
/// ssa_slots_copy buffer.
#[test]
fn invoke_with_resume_exposes_deopt_snapshot_to_fallback() {
    let fn_id = 722u32;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(fn_id);
    // Body padded past `TINY_TRACE_OP_THRESHOLD` (uses `+ 0`
    // tails so the trace stays semantically `x + y`) so the
    // runtime gate doesn't skip the trace before its overflow
    // guard can fire.
    register_recording(
        fn_id,
        RecordingRegistration {
            body: vec![
                TaggedOp {
                    op: Op::LocalGet(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::LocalGet(1),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Add(IrType::I64),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::ConstI64(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Add(IrType::I64),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::ConstI64(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Add(IrType::I64),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            param_tys: vec![IrType::I32, IrType::I32],
        },
    );
    let warm_args: [u64; 2] = [1u64, 2u64];
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, warm_args.as_ptr());
    }
    assert!(state.lookup_trace(fn_id).is_some(), "trace must install");

    // Trigger overflow to force a deopt; capture the snapshot.
    let of_args: [u64; 2] = [i64::MAX as u64, 1u64];
    let snapshot_was_present = std::cell::Cell::new(false);
    let snapshot_slot_count = std::cell::Cell::new(0usize);
    let snapshot_pc = std::cell::Cell::new(0u64);
    let fallback_args_ptr = std::cell::Cell::new(std::ptr::null::<u64>());
    let r = unsafe {
        state.invoke_with_resume(fn_id, of_args.as_ptr(), 32, |args, resume_pc, snapshot| {
            fallback_args_ptr.set(args);
            if let Some(s) = snapshot {
                snapshot_was_present.set(true);
                snapshot_slot_count.set(s.ssa_slots_copy.len());
                snapshot_pc.set(resume_pc.unwrap_or(0));
            }
            // Stand-in for resume_from_pc: return the wrapping sum.
            i64::MAX.wrapping_add(1) as u64
        })
    };
    assert_eq!(r, i64::MIN as u64);
    assert!(
        snapshot_was_present.get(),
        "GuardFailed must surface a DeoptStateSnapshot"
    );
    assert!(
        snapshot_slot_count.get() > 0,
        "snapshot must carry ssa_slots_copy"
    );
    assert_eq!(
        fallback_args_ptr.get(),
        of_args.as_ptr(),
        "args_ptr must round-trip into the fallback unchanged"
    );
    // resume_pc may be 0 on the very first guard site (recorder's
    // monotone counter); we don't pin the exact value. The
    // important property is that the snapshot is populated and the
    // slots survived the trace -> fallback boundary.

    let _ = clear_recording(fn_id);
    let _ = state.invalidate_trace(fn_id);
}

/// v6-δ M1 R5: deopt path now dispatches `save_deopt` through
/// `ctx.host_hooks.save_deopt` via `call_indirect`. A test that stubs
/// the slot with a custom function observes the call (and confirms
/// the indirect dispatch path is hot, not the legacy direct extern
/// call).
#[test]
fn save_deopt_dispatches_through_host_hooks_table() {
    use relon_trace_abi::{HostHookTable, TraceSaveDeoptFn};

    // Per-thread observation slot the custom save_deopt writes into.
    thread_local! {
        static CUSTOM_OBSERVED: std::cell::Cell<(u32, u64)> = const {
            std::cell::Cell::new((0, 0))
        };
    }
    unsafe extern "C" fn custom_save_deopt(
        ctx: *mut TraceContext,
        guard_pc: u32,
        external_pc: u64,
    ) {
        // Record into thread-local + also populate ctx.deopt_state
        // so the trace's return path keeps observing the snapshot.
        CUSTOM_OBSERVED.with(|c| c.set((guard_pc, external_pc)));
        unsafe {
            relon_trace_jit::runtime::__relon_trace_save_deopt(ctx, guard_pc, external_pc);
        }
    }

    let fn_id = 723u32;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body: vec![
                TaggedOp {
                    op: Op::LocalGet(0),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::LocalGet(1),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Add(IrType::I64),
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            param_tys: vec![IrType::I32, IrType::I32],
        },
    );
    let warm: [u64; 2] = [1, 2];
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, warm.as_ptr());
    }
    let trace_fn = state.lookup_trace(fn_id).expect("trace");

    // Trigger overflow with a custom HostHookTable that observes the
    // call. The trace's deopt block dispatches through this slot.
    let hooks = HostHookTable {
        save_deopt: Some(custom_save_deopt as TraceSaveDeoptFn),
        ..HostHookTable::default()
    };
    let mut ctx = TraceContext::with_hooks(32, hooks);
    let of_args: [u64; 2] = [i64::MAX as u64, 1];
    CUSTOM_OBSERVED.with(|c| c.set((0, 0)));
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, of_args.as_ptr()) };
    assert!(matches!(status, TraceEntryStatus::GuardFailed));
    let (observed_pc, observed_external_pc) = CUSTOM_OBSERVED.with(|c| c.get());
    assert!(
        observed_pc != 0 || observed_external_pc != 0,
        "custom save_deopt must have been called via call_indirect through ctx.host_hooks"
    );
    assert!(
        ctx.deopt_state.is_some(),
        "custom save_deopt should still populate ctx.deopt_state"
    );

    let _ = clear_recording(fn_id);
    let _ = state.invalidate_trace(fn_id);
}

/// v6-δ M1 R4: tree-walker now registers `abs` / `min` / `max` /
/// `clamp` as bare free fns and `length` / `is_empty` / `concat` /
/// `substring` / `starts_with` / `sum` / `max` as String/List methods,
/// closing the FunctionNotFound gap the v6-γ M5 corpus surfaced.
#[test]
fn tree_walker_stdlib_free_fn_surface() {
    use relon::{new_evaluator, Backend};

    let cases: &[(&str, Value)] = &[
        ("#main(Int x) -> Int\nabs(x)", Value::Int(42)),
        ("#main() -> Int\n\"hello\".length()", Value::Int(5)),
        ("#main() -> Bool\n\"\".is_empty()", Value::Bool(true)),
        ("#main() -> Bool\n\"hi\".is_empty()", Value::Bool(false)),
        (
            "#main() -> String\n\"foo\".concat(\"bar\")",
            Value::String("foobar".into()),
        ),
        // substring(s, start, len) — `(1, 3)` selects "ell".
        (
            "#main() -> String\n\"hello\".substring(1, 3)",
            Value::String("ell".into()),
        ),
        (
            "#main() -> Bool\n\"hello world\".starts_with(\"hello\")",
            Value::Bool(true),
        ),
        ("#main() -> Int\n[1, 2, 3, 4, 5].sum()", Value::Int(15)),
        (
            "#main() -> Int\n[3, 1, 4, 1, 5, 9, 2, 6].max()",
            Value::Int(9),
        ),
    ];
    for (src, expected) in cases {
        let args = if src.contains("Int x") {
            let mut m = HashMap::new();
            m.insert("x".to_string(), Value::Int(-42));
            m
        } else {
            HashMap::new()
        };
        let ev = new_evaluator(src, Backend::TreeWalk).expect("setup");
        let r = ev
            .run_main(args)
            .unwrap_or_else(|e| panic!("{src} -> {e:?}"));
        assert_eq!(&r, expected, "tree-walker stdlib surface for {src}");
    }
}

/// v6-δ M1 R3: `Evaluator::resume_from_pc` default implementation
/// re-runs `run_main` so the 4-prong sandbox semantics keep holding
/// when a deopt'd trace bounces back into the tree-walker. We exercise
/// the div-by-zero trap path since it's a representative sandbox
/// surface; bounds-check / capability-gate / resource-limit follow
/// the same shape (the default forwards to run_main, which already
/// enforces all four).
#[test]
fn evaluator_resume_from_pc_default_preserves_sandbox_semantics() {
    use relon_eval_api::Evaluator;
    use std::sync::Arc;
    let source = "#main(Int x, Int y) -> Int\nx / y";
    let node = relon_parser::parse_document(source).expect("parse");
    let analyzed = Arc::new(relon_analyzer::analyze(&node));
    let mut ctx = relon_evaluator::Context::new()
        .with_root(node)
        .with_analyzed(Arc::clone(&analyzed));
    relon_evaluator::TreeWalkEvaluator::prepare_in_place(&mut ctx);
    let ev: Box<dyn Evaluator> = Box::new(relon_evaluator::TreeWalkEvaluator::new(Arc::new(ctx)));

    // 1. Happy path through resume_from_pc — should return 21.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(42));
    args.insert("y".to_string(), Value::Int(2));
    let r = ev
        .resume_from_pc(args, /*external_pc*/ 0, /*slots*/ &[])
        .expect("ok");
    assert_eq!(r, Value::Int(21));

    // 2. Div-by-zero trap — resume_from_pc default forwards to
    //    run_main which preserves the sandbox semantics.
    let mut bad = HashMap::new();
    bad.insert("x".to_string(), Value::Int(5));
    bad.insert("y".to_string(), Value::Int(0));
    let err = ev
        .resume_from_pc(bad, /*external_pc*/ 0xdeadbeef, /*slots*/ &[0, 0])
        .expect_err("must trap");
    assert!(
        matches!(err, relon_eval_api::RuntimeError::DivisionByZero(_)),
        "expected DivisionByZero on resume, got {err:?}"
    );
}

#[test]
fn jump_helper_aborts_recording_for_unsupported_op() {
    // Register a body containing an op outside the recorder's
    // accepted envelope. The helper should walk in, abort, and not
    // install any trace.
    //
    // Note: the assertion is scoped to *this* `fn_id` only. Other
    // tests in this binary share `global_trace_jit_state()` and may
    // install / invalidate their own fn_ids concurrently with this
    // body, so a global `installed_count()` delta is not a reliable
    // signal here. The recorder's correctness invariant is "do not
    // install a trace for fn_id 703 once we abort", which `lookup_trace`
    // captures directly.
    let fn_id = 703u32;
    let _ = clear_recording(fn_id);
    let state = global_trace_jit_state();
    // Best-effort reset of any sticky install from a previous run
    // in the same process (e.g. nextest's process reuse).
    let _ = state.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body: vec![
                // `CallNative` always classifies as Unrecoverable in
                // the recorder's lowering rule, so this aborts on
                // the first op.
                TaggedOp {
                    op: Op::CallNative {
                        import_idx: 0,
                        param_tys: vec![],
                        ret_ty: IrType::I64,
                        cap_bit: 0,
                    },
                    range: TokenRange::default(),
                },
                TaggedOp {
                    op: Op::Return,
                    range: TokenRange::default(),
                },
            ],
            param_tys: vec![],
        },
    );
    unsafe {
        relon_codegen_native::trace_install::__relon_jump_to_recorder(fn_id, std::ptr::null());
    }
    assert!(state.lookup_trace(fn_id).is_none(), "aborted → no install");
    let _ = clear_recording(fn_id);
}
