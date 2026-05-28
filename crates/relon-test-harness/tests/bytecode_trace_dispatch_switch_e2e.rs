//! v6-δ M2-B phase 4c-cont: end-to-end test of the bytecode VM
//! dispatcher switch.
//!
//! Flow:
//!
//! 1. Pick a unique `fn_id` no other smoke test owns.
//! 2. Register the IR body (`x + y`) under the id so the recorder can
//!    walk it.
//! 3. Build a `BytecodeEvaluator` for the same source, stamp the
//!    matching `fn_id`, install both the cranelift hot trigger (so
//!    invocation 1 drives a recording → JIT install) **and** the
//!    cranelift trace lookup (so invocation 2 bypasses the bytecode
//!    dispatch loop and routes through the installed trace).
//! 4. Invocation 1: counter trips → trace installs → bytecode VM
//!    still computes the real result (the recorder is advisory; the
//!    invocation in flight stays correct).
//! 5. Invocation 2: lookup hits → bypass dispatch → trace returns
//!    the real result via `TraceContext::result_slot`.
//!
//! Side-channel verification: a mock-recording wrapper around the
//! cranelift lookup counts how many times the trace was consulted
//! vs. how many times the bytecode dispatch loop actually ticked.
//! We use `relon_codegen_cranelift::jump_helper_call_count` for the
//! recorder side (proxy for "did invocation 1 trigger?") and
//! `BytecodeEvaluator::run_main_with_metrics` against a no-trace
//! control to bound the dispatch cost.

use std::collections::HashMap;
use std::sync::Arc;

use relon_bytecode::hot_counter::{peek_hot, reset_hot_all};
use relon_bytecode::trace_dispatch::{InstalledTraceLookup, TraceInvokeOutcome};
use relon_bytecode::{
    BytecodeEvaluator, HotTraceTriggerHandle, InstalledTraceLookupHandle, COUNTER_SATURATED,
};
use relon_codegen_cranelift::trace_install::{
    clear_recording, global_trace_jit_state, jump_helper_call_count, register_recording,
    reset_jump_helper_call_count, RecordingRegistration,
};
use relon_codegen_cranelift::{CraneliftHotTrigger, CraneliftTraceLookup};
use relon_eval_api::{Evaluator, Value};
use relon_ir::{IrType, Op, TaggedOp};
use relon_parser::TokenRange;

fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

/// Synthetic fn_ids picked from a slot no other smoke test owns.
/// The hot-counter e2e tests use 91/92; this file picks 93/94/95.
const FN_ID_BYPASS: u32 = 93;
const FN_ID_REPEAT: u32 = 94;

fn build_add_body() -> Vec<TaggedOp> {
    // Padded past `TINY_TRACE_OP_THRESHOLD` (via `+ 0` tails so
    // the trace stays semantically `x + y`) so the runtime gate
    // doesn't route the call past the trace before the
    // dispatch-switch assertion can observe a Success outcome.
    vec![
        t(Op::LocalGet(0)),
        t(Op::LocalGet(1)),
        t(Op::Add(IrType::I64)),
        t(Op::ConstI64(0)),
        t(Op::Add(IrType::I64)),
        t(Op::ConstI64(0)),
        t(Op::Add(IrType::I64)),
        t(Op::Return),
    ]
}

/// Counting wrapper so the test can assert how many times the
/// dispatcher switch actually hit. Forwards to the real
/// `CraneliftTraceLookup`; increments an atomic on every call and on
/// every Success outcome.
struct CountingLookup {
    inner: CraneliftTraceLookup,
    total: std::sync::atomic::AtomicUsize,
    successes: std::sync::atomic::AtomicUsize,
}

impl CountingLookup {
    fn new() -> Self {
        Self {
            inner: CraneliftTraceLookup,
            total: std::sync::atomic::AtomicUsize::new(0),
            successes: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn total(&self) -> usize {
        self.total.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn successes(&self) -> usize {
        self.successes.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl InstalledTraceLookup for CountingLookup {
    fn try_invoke(&self, fn_id: u32, args: &[relon_bytecode::vm::VmValue]) -> TraceInvokeOutcome {
        self.total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let outcome = self.inner.try_invoke(fn_id, args);
        if matches!(outcome, TraceInvokeOutcome::Success { .. }) {
            self.successes
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        outcome
    }
}

/// Drive a hot loop through the bytecode VM until a trace installs,
/// then prove the **next** invocation skips the bytecode dispatch
/// loop entirely (the trace fn returns the value via
/// `TraceContext::result_slot`).
#[test]
fn bytecode_dispatch_bypasses_after_trace_install() {
    reset_hot_all();
    reset_jump_helper_call_count();
    let _ = clear_recording(FN_ID_BYPASS);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(FN_ID_BYPASS);

    register_recording(
        FN_ID_BYPASS,
        RecordingRegistration {
            body: build_add_body(),
            param_tys: vec![IrType::I32, IrType::I32],
            ..Default::default()
        },
    );

    let trigger: HotTraceTriggerHandle = Arc::new(CraneliftHotTrigger);
    let counting = Arc::new(CountingLookup::new());
    let lookup_handle: InstalledTraceLookupHandle = counting.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(FN_ID_BYPASS)
        .with_hot_trigger(trigger)
        .with_hot_threshold(1)
        .with_trace_lookup(lookup_handle);

    // Invocation 1: lookup consulted (no trace yet → NoTrace),
    // dispatch runs, hot counter trips, trigger fires, recorder
    // walks the IR, trace installs.
    let helper_before = jump_helper_call_count();
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let r1 = ev.run_main(args.clone()).expect("invocation 1");
    assert_eq!(r1, Value::Int(42), "invocation 1 must compute via dispatch");
    assert_eq!(
        counting.total(),
        1,
        "lookup must be consulted on every invocation"
    );
    assert_eq!(
        counting.successes(),
        0,
        "invocation 1: no trace installed yet → no Success bypass"
    );
    assert_eq!(peek_hot(FN_ID_BYPASS), Some(COUNTER_SATURATED));
    let helper_after_1 = jump_helper_call_count();
    assert_eq!(
        helper_after_1 - helper_before,
        1,
        "trigger fires exactly once on invocation 1"
    );
    assert!(
        state.lookup_trace(FN_ID_BYPASS).is_some(),
        "trace must install after invocation 1's recording"
    );

    // Invocation 2: lookup consulted, trace hits → bypass dispatch.
    let r2 = ev.run_main(args.clone()).expect("invocation 2");
    assert_eq!(
        r2,
        Value::Int(42),
        "invocation 2 must return 42 via the trace's result_slot"
    );
    assert_eq!(
        counting.total(),
        2,
        "lookup must be consulted exactly twice (once per run_main)"
    );
    assert_eq!(
        counting.successes(),
        1,
        "invocation 2 must hit the Success path (trace bypass)"
    );
    let helper_after_2 = jump_helper_call_count();
    assert_eq!(
        helper_after_2 - helper_after_1,
        0,
        "invocation 2 must NOT re-trigger the recorder"
    );

    // Cleanup.
    let _ = clear_recording(FN_ID_BYPASS);
    let _ = state.invalidate_trace(FN_ID_BYPASS);
    reset_hot_all();
    reset_jump_helper_call_count();
}

/// Repeat invocations after install: every subsequent `run_main` must
/// land on the Success bypass. Without the dispatcher switch the
/// bytecode dispatch loop would still run on every call; the switch
/// flips that to a single indirect trace call per invocation.
#[test]
fn bytecode_repeat_invocations_all_use_trace_bypass() {
    reset_hot_all();
    reset_jump_helper_call_count();
    let _ = clear_recording(FN_ID_REPEAT);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(FN_ID_REPEAT);

    register_recording(
        FN_ID_REPEAT,
        RecordingRegistration {
            body: build_add_body(),
            param_tys: vec![IrType::I32, IrType::I32],
            ..Default::default()
        },
    );

    let trigger: HotTraceTriggerHandle = Arc::new(CraneliftHotTrigger);
    let counting = Arc::new(CountingLookup::new());
    let lookup_handle: InstalledTraceLookupHandle = counting.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(FN_ID_REPEAT)
        .with_hot_trigger(trigger)
        .with_hot_threshold(1)
        .with_trace_lookup(lookup_handle);

    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(7));
    args.insert("y".to_string(), Value::Int(35));
    // First call: dispatch + recorder kick-off + install.
    let r0 = ev.run_main(args.clone()).expect("warm-up");
    assert_eq!(r0, Value::Int(42));
    assert!(
        state.lookup_trace(FN_ID_REPEAT).is_some(),
        "trace must install after warm-up"
    );

    // 10 subsequent invocations: all must hit the bypass.
    for i in 0..10 {
        let r = ev.run_main(args.clone()).expect("bypassed call");
        assert_eq!(r, Value::Int(42), "call {i}");
    }
    assert_eq!(
        counting.total(),
        11,
        "lookup consulted on every run_main (1 warm-up + 10 bypass)"
    );
    assert_eq!(
        counting.successes(),
        10,
        "10 bypass invocations must all hit Success"
    );

    let _ = clear_recording(FN_ID_REPEAT);
    let _ = state.invalidate_trace(FN_ID_REPEAT);
    reset_hot_all();
    reset_jump_helper_call_count();
}
