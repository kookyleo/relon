//! v6-δ M2-B phase 4c-cont sub-task B: end-to-end test of the full
//! bytecode -> trace -> deopt -> bytecode handoff via the dispatcher
//! switch.
//!
//! The phase 4c-cont sub-task A test
//! (`bytecode_trace_dispatch_switch_e2e`) covers the happy path —
//! trace returns Success and the bytecode VM uses the result_slot.
//! This file covers the cold path: the trace fires an internal guard
//! (overflow) on cold args, the dispatcher switch sees the Deopt
//! outcome, the snapshot routes through `resume_from_snapshot`, and
//! the bytecode VM resumes from the snapshot's `external_pc`.
//!
//! The user-visible contract: `run_main` on cold args returns the
//! same `RuntimeError` envelope as the bytecode VM would have
//! produced without the trace in the picture. The trace's role is a
//! pure optimisation; correctness comes from the bytecode body.

use std::collections::HashMap;
use std::sync::Arc;

use relon_bytecode::hot_counter::{peek_hot, reset_hot_all};
use relon_bytecode::trace_dispatch::{InstalledTraceLookup, TraceInvokeOutcome};
use relon_bytecode::{
    BytecodeEvaluator, HotTraceTriggerHandle, InstalledTraceLookupHandle, COUNTER_SATURATED,
};
use relon_codegen_native::trace_install::{
    clear_recording, global_trace_jit_state, register_recording, reset_jump_helper_call_count,
    RecordingRegistration,
};
use relon_codegen_native::{CraneliftHotTrigger, CraneliftTraceLookup};
use relon_eval_api::{Evaluator, RuntimeError, Value};
use relon_ir::{IrType, Op, TaggedOp};
use relon_parser::TokenRange;

fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

const FN_ID_HANDOFF: u32 = 95;
const FN_ID_HANDOFF_WARM: u32 = 96;

fn build_add_body() -> Vec<TaggedOp> {
    vec![
        t(Op::LocalGet(0)),
        t(Op::LocalGet(1)),
        t(Op::Add(IrType::I64)),
        t(Op::Return),
    ]
}

/// Wrap the cranelift lookup so the test can count how many times
/// each outcome variant fired.
struct OutcomeCountingLookup {
    inner: CraneliftTraceLookup,
    success: std::sync::atomic::AtomicUsize,
    deopt: std::sync::atomic::AtomicUsize,
    no_trace: std::sync::atomic::AtomicUsize,
}

impl OutcomeCountingLookup {
    fn new() -> Self {
        Self {
            inner: CraneliftTraceLookup,
            success: std::sync::atomic::AtomicUsize::new(0),
            deopt: std::sync::atomic::AtomicUsize::new(0),
            no_trace: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn success(&self) -> usize {
        self.success.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn deopt(&self) -> usize {
        self.deopt.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn no_trace(&self) -> usize {
        self.no_trace.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl InstalledTraceLookup for OutcomeCountingLookup {
    fn try_invoke(&self, fn_id: u32, args: &[relon_bytecode::vm::VmValue]) -> TraceInvokeOutcome {
        let outcome = self.inner.try_invoke(fn_id, args);
        match &outcome {
            TraceInvokeOutcome::Success { .. } => {
                self.success
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            TraceInvokeOutcome::Deopt { .. } => {
                self.deopt
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            TraceInvokeOutcome::NoTrace => {
                self.no_trace
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        outcome
    }
}

/// Cold path: a trace recorded against non-overflowing inputs deopts
/// when called with `(i64::MAX, 1)`; the bytecode dispatcher switch
/// catches the deopt and routes through `resume_from_snapshot`. The
/// bytecode VM's Add op re-overflows and surfaces
/// `RuntimeError::NumericOverflow` — exactly the envelope a plain
/// `run_main` call (without the trace) would have produced.
#[test]
fn deopt_handoff_propagates_bytecode_overflow_envelope() {
    reset_hot_all();
    reset_jump_helper_call_count();
    let _ = clear_recording(FN_ID_HANDOFF);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(FN_ID_HANDOFF);

    register_recording(
        FN_ID_HANDOFF,
        RecordingRegistration {
            body: build_add_body(),
            param_tys: vec![IrType::I32, IrType::I32],
        },
    );

    let trigger: HotTraceTriggerHandle = Arc::new(CraneliftHotTrigger);
    let counting = Arc::new(OutcomeCountingLookup::new());
    let lookup_handle: InstalledTraceLookupHandle = counting.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(FN_ID_HANDOFF)
        .with_hot_trigger(trigger)
        .with_hot_threshold(1)
        .with_trace_lookup(lookup_handle);

    // Warm-up: drive the recorder with non-overflowing args, install
    // the trace.
    let mut warm_args = HashMap::new();
    warm_args.insert("x".to_string(), Value::Int(1));
    warm_args.insert("y".to_string(), Value::Int(2));
    let warm = ev.run_main(warm_args).expect("warm-up");
    assert_eq!(warm, Value::Int(3));
    assert!(
        state.lookup_trace(FN_ID_HANDOFF).is_some(),
        "trace must install after warm-up"
    );
    assert_eq!(peek_hot(FN_ID_HANDOFF), Some(COUNTER_SATURATED));

    // Sanity baseline: without the trace, the bytecode VM would
    // produce NumericOverflow on (i64::MAX, 1). Verify this through
    // a sibling evaluator with no trace lookup installed (so we have
    // a known baseline envelope to compare the handoff against).
    let bare = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile");
    let mut overflow_args = HashMap::new();
    overflow_args.insert("x".to_string(), Value::Int(i64::MAX));
    overflow_args.insert("y".to_string(), Value::Int(1));
    let bare_err = bare
        .run_main(overflow_args.clone())
        .expect_err("bare bytecode must trap on overflow");
    assert!(
        matches!(bare_err, RuntimeError::NumericOverflow(_)),
        "bare bytecode VM envelope should be NumericOverflow, got {bare_err:?}"
    );

    // Cold path: same args through the trace-enabled evaluator. The
    // trace's overflow guard fires, the dispatcher switch routes
    // Deopt through resume_from_snapshot, and the bytecode VM
    // re-attempts the Add → traps the same way.
    let cold_err = ev
        .run_main(overflow_args)
        .expect_err("trace handoff must end in the same trap");
    assert!(
        matches!(cold_err, RuntimeError::NumericOverflow(_)),
        "handoff envelope must match bare bytecode, got {cold_err:?}"
    );

    // Outcome accounting:
    // - warm-up: NoTrace (no trace installed yet at top of call).
    // - cold: Deopt (trace installed; guard fires).
    assert_eq!(counting.no_trace(), 1, "warm-up: NoTrace");
    assert_eq!(counting.deopt(), 1, "cold: Deopt routed through switch");
    assert_eq!(counting.success(), 0, "no Success outcomes in this test");

    let _ = clear_recording(FN_ID_HANDOFF);
    let _ = state.invalidate_trace(FN_ID_HANDOFF);
    reset_hot_all();
    reset_jump_helper_call_count();
}

/// Mixed workload: warm-up + 3 successful invocations + 1 cold deopt.
/// Asserts the outcome counter sees the full N+1+1 shape (1 NoTrace
/// warm-up, 3 Success, 1 Deopt) — the dispatcher switch routes each
/// outcome to the right downstream path.
#[test]
fn deopt_handoff_mixed_workload_routes_each_outcome() {
    reset_hot_all();
    reset_jump_helper_call_count();
    let _ = clear_recording(FN_ID_HANDOFF_WARM);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(FN_ID_HANDOFF_WARM);

    register_recording(
        FN_ID_HANDOFF_WARM,
        RecordingRegistration {
            body: build_add_body(),
            param_tys: vec![IrType::I32, IrType::I32],
        },
    );

    let trigger: HotTraceTriggerHandle = Arc::new(CraneliftHotTrigger);
    let counting = Arc::new(OutcomeCountingLookup::new());
    let lookup_handle: InstalledTraceLookupHandle = counting.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(FN_ID_HANDOFF_WARM)
        .with_hot_trigger(trigger)
        .with_hot_threshold(1)
        .with_trace_lookup(lookup_handle);

    let mut warm = HashMap::new();
    warm.insert("x".to_string(), Value::Int(10));
    warm.insert("y".to_string(), Value::Int(20));
    // Warm-up: dispatch + install.
    let r = ev.run_main(warm.clone()).expect("warm-up");
    assert_eq!(r, Value::Int(30));

    // 3 Successful bypasses.
    for _ in 0..3 {
        let r = ev.run_main(warm.clone()).expect("hot bypass");
        assert_eq!(r, Value::Int(30));
    }

    // 1 Cold deopt + bytecode-resume re-trap.
    let mut cold = HashMap::new();
    cold.insert("x".to_string(), Value::Int(i64::MAX));
    cold.insert("y".to_string(), Value::Int(1));
    let err = ev.run_main(cold).expect_err("must trap");
    assert!(matches!(err, RuntimeError::NumericOverflow(_)));

    assert_eq!(counting.no_trace(), 1, "warm-up: NoTrace");
    assert_eq!(counting.success(), 3, "3 hot Success bypasses");
    assert_eq!(counting.deopt(), 1, "1 cold Deopt routed through resume");

    let _ = clear_recording(FN_ID_HANDOFF_WARM);
    let _ = state.invalidate_trace(FN_ID_HANDOFF_WARM);
    reset_hot_all();
    reset_jump_helper_call_count();
}
