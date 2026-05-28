//! v6-δ M2-B phase 4c: end-to-end test of the bytecode VM hot-counter
//! prologue → CraneliftHotTrigger → trace recorder → JIT install
//! pipeline.
//!
//! Flow:
//!
//! 1. Pick a `fn_id` that no other smoke test owns.
//! 2. Register the IR body (`x + y` / `LocalGet; LocalGet; Add; Return`)
//!    under the id so `__relon_jump_to_recorder` has a walkable body
//!    when the bytecode trigger forwards the event.
//! 3. Build a `BytecodeEvaluator` for the same source, stamp the
//!    matching `fn_id`, install the cranelift adapter as the hot
//!    trigger, set the threshold to 1 so the trigger fires on the
//!    first invocation.
//! 4. Call `run_main` once.
//! 5. Assert (a) the bytecode VM's `peek_hot(FN_ID)` reports
//!    `COUNTER_SATURATED` (the prologue ran and the counter tripped);
//!    (b) `jump_helper_call_count()` increased by exactly one (the
//!    adapter forwarded the event to the recorder); (c)
//!    `global_trace_jit_state().lookup_trace(FN_ID)` returns
//!    `Some(_)` — the recorder walked the registered IR body, the
//!    optimiser / emitter / install pipeline ran, and the trace fn
//!    is now resident.

use std::collections::HashMap;
use std::sync::Arc;

use relon_bytecode::hot_counter::{peek_hot, reset_hot_all};
use relon_bytecode::{BytecodeEvaluator, HotTraceTriggerHandle, COUNTER_SATURATED};
use relon_codegen_cranelift::trace_install::{
    clear_recording, global_trace_jit_state, jump_helper_call_count, register_recording,
    reset_jump_helper_call_count, RecordingRegistration,
};
use relon_codegen_cranelift::CraneliftHotTrigger;
use relon_eval_api::{Evaluator, Value};
use relon_ir::{IrType, Op, TaggedOp};
use relon_parser::TokenRange;

fn t(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

/// Synthetic fn_id picked from a slot no other smoke test owns. The
/// `bytecode_deopt_integration` tests use 67/68/69; trace_jit_smoke
/// stays ≤ 803; three-way allocates from `MAX_FN_ID / 2 = 512`. Pick
/// something out of all three ranges so concurrent test runs (cargo
/// test --workspace) don't interfere via the shared thread-locals.
const FN_ID: u32 = 91;

/// Build the IR body that maps cleanly onto the bytecode VM's `x + y`
/// compile output. The recorder walks this body on the trigger; the
/// resulting trace lowers to a single Add op, which the JIT installs.
fn build_add_body() -> Vec<TaggedOp> {
    vec![
        t(Op::LocalGet(0)),
        t(Op::LocalGet(1)),
        t(Op::Add(IrType::I64)),
        t(Op::Return),
    ]
}

#[test]
fn bytecode_hot_loop_drives_trace_install() {
    // Test-state hygiene: reset the per-thread counters + jump-helper
    // call count + any prior recording / install for the chosen fn_id.
    reset_hot_all();
    reset_jump_helper_call_count();
    let _ = clear_recording(FN_ID);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(FN_ID);

    // Register the IR body for the recorder. The bytecode side
    // forwards the (fn_id, args) pair to `__relon_jump_to_recorder`;
    // without a registration the helper logs + returns and the trace
    // never installs.
    register_recording(
        FN_ID,
        RecordingRegistration {
            body: build_add_body(),
            param_tys: vec![IrType::I32, IrType::I32],
            ..Default::default()
        },
    );

    // Build the bytecode evaluator. `from_source` runs the parse →
    // analyze → IR lower → compile pipeline; we then stamp the same
    // `fn_id` the recorder is registered for and install the
    // cranelift trigger.
    let source = "#main(Int x, Int y) -> Int\nx + y";
    let trigger: HotTraceTriggerHandle = Arc::new(CraneliftHotTrigger);
    let ev = BytecodeEvaluator::from_source(source)
        .expect("bytecode compile")
        .with_fn_id(FN_ID)
        .with_hot_trigger(trigger)
        // Threshold 1 → fire on the first invocation. Production
        // configs use the default 1000.
        .with_hot_threshold(1);

    // Capture the jump-helper count before the hot invocation so the
    // delta assertion is robust against other tests pre-warming the
    // counter on the same thread.
    let helper_before = jump_helper_call_count();

    // Run `run_main` once. The prologue bumps the slot, the counter
    // crosses the threshold of 1, the trigger fires, the cranelift
    // adapter forwards to `__relon_jump_to_recorder`, the recorder
    // walks the registered IR body, the optimiser + emitter + install
    // pipeline runs, the trace fn lands in the global registry.
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let result = ev.run_main(args).expect("run_main");
    assert_eq!(
        result,
        Value::Int(42),
        "bytecode VM still returns the real value"
    );

    // Assertion a: hot counter saturated.
    assert_eq!(
        peek_hot(FN_ID),
        Some(COUNTER_SATURATED),
        "hot counter must saturate after the threshold-1 trigger fires"
    );

    // Assertion b: jump helper invoked exactly once.
    let helper_after = jump_helper_call_count();
    assert_eq!(
        helper_after - helper_before,
        1,
        "CraneliftHotTrigger should forward the bytecode trigger exactly once"
    );

    // Assertion c: a trace landed for FN_ID.
    let installed = state.lookup_trace(FN_ID);
    assert!(
        installed.is_some(),
        "trace must be installed after the full recorder pipeline"
    );

    // Cleanup so subsequent runs of this test (or sibling tests on
    // the same thread) start from a clean slot.
    let _ = clear_recording(FN_ID);
    let _ = state.invalidate_trace(FN_ID);
    reset_hot_all();
    reset_jump_helper_call_count();
}

#[test]
fn bytecode_hot_loop_subsequent_invocations_skip_trigger() {
    // Second sanity: once the slot saturates and a trace is installed,
    // further invocations must NOT re-fire the trigger. This guards
    // against the "every invocation reruns the recorder" anti-pattern
    // the saturated-slot short circuit prevents.
    reset_hot_all();
    reset_jump_helper_call_count();
    const ID: u32 = 92;
    let _ = clear_recording(ID);
    let state = global_trace_jit_state();
    let _ = state.invalidate_trace(ID);

    register_recording(
        ID,
        RecordingRegistration {
            body: build_add_body(),
            param_tys: vec![IrType::I32, IrType::I32],
            ..Default::default()
        },
    );

    let source = "#main(Int x, Int y) -> Int\nx + y";
    let trigger: HotTraceTriggerHandle = Arc::new(CraneliftHotTrigger);
    let ev = BytecodeEvaluator::from_source(source)
        .expect("bytecode compile")
        .with_fn_id(ID)
        .with_hot_trigger(trigger)
        .with_hot_threshold(1);

    let helper_before = jump_helper_call_count();
    for _ in 0..5 {
        let mut args = HashMap::new();
        args.insert("x".to_string(), Value::Int(1));
        args.insert("y".to_string(), Value::Int(1));
        let _ = ev.run_main(args);
    }
    let helper_after = jump_helper_call_count();
    assert_eq!(
        helper_after - helper_before,
        1,
        "trigger must fire exactly once across 5 invocations"
    );
    assert_eq!(peek_hot(ID), Some(COUNTER_SATURATED));

    let _ = clear_recording(ID);
    let _ = state.invalidate_trace(ID);
    reset_hot_all();
    reset_jump_helper_call_count();
}
