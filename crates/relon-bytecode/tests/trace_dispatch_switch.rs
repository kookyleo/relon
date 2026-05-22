//! M2-B phase 4c-cont: integration tests for the dispatcher switch.
//!
//! These tests pin the bytecode-side behaviour without the cranelift
//! install pipeline. A mock [`InstalledTraceLookup`] returns canned
//! [`TraceInvokeOutcome`] variants; the assertions verify that
//! `BytecodeEvaluator::run_main` routes each variant correctly:
//!
//! - `NoTrace`  → the bytecode dispatch loop runs to completion.
//! - `Success { result }` → the dispatch loop is skipped entirely and
//!   the trace's `result_slot` value becomes the [`Value`] return.
//! - `Deopt { snapshot }` → the resume path takes over and produces
//!   the same final value the bytecode VM would have computed.
//!
//! The end-to-end test that exercises the real `CraneliftTraceLookup`
//! against a JIT-installed trace lives in
//! `relon-test-harness::tests::bytecode_trace_dispatch_switch_e2e`
//! (added alongside this file).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use relon_bytecode::trace_dispatch::{
    InstalledTraceLookup, InstalledTraceLookupHandle, TraceInvokeOutcome,
};
use relon_bytecode::vm::VmValue;
use relon_bytecode::BytecodeEvaluator;
use relon_eval_api::{Evaluator, Value};
use relon_trace_abi::DeoptStateSnapshot;

/// Configurable test lookup. Records every (`fn_id`, `args`) pair and
/// pops a canned outcome off the front of `responses` per invocation.
/// If responses run dry the lookup falls back to `NoTrace` so the
/// bytecode VM still produces a defined value.
struct MockLookup {
    log: Mutex<Vec<(u32, Vec<VmValue>)>>,
    responses: Mutex<std::collections::VecDeque<TraceInvokeOutcome>>,
}

impl MockLookup {
    fn new(responses: Vec<TraceInvokeOutcome>) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        })
    }

    fn call_count(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    fn calls(&self) -> Vec<(u32, Vec<VmValue>)> {
        self.log.lock().unwrap().clone()
    }
}

impl InstalledTraceLookup for MockLookup {
    fn try_invoke(&self, fn_id: u32, args: &[VmValue]) -> TraceInvokeOutcome {
        self.log.lock().unwrap().push((fn_id, args.to_vec()));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(TraceInvokeOutcome::NoTrace)
    }
}

/// `NoTrace` short-circuit: the bytecode dispatch loop runs and the
/// final value matches a plain `run_main` (no lookup installed).
#[test]
fn no_trace_outcome_falls_through_to_dispatch() {
    let lookup = MockLookup::new(vec![TraceInvokeOutcome::NoTrace]);
    let handle: InstalledTraceLookupHandle = lookup.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(1001)
        .with_trace_lookup(handle);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let result = ev.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42), "dispatch must produce real sum");
    assert_eq!(
        lookup.call_count(),
        1,
        "lookup is consulted exactly once at run_main entry"
    );
    let calls = lookup.calls();
    assert_eq!(calls[0].0, 1001);
    assert_eq!(calls[0].1, vec![40u64, 2u64]);
}

/// `Success { result }` bypass: the trace's `result_slot` becomes the
/// return value and the bytecode dispatch loop never runs. Verify by
/// having the trace return a value the bytecode dispatch could NOT
/// have produced (i.e. a value unrelated to `x + y`).
#[test]
fn success_outcome_bypasses_dispatch_and_returns_trace_result() {
    let lookup = MockLookup::new(vec![TraceInvokeOutcome::Success { result: 9999 }]);
    let handle: InstalledTraceLookupHandle = lookup.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(1002)
        .with_trace_lookup(handle);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let result = ev.run_main(args).expect("run_main");
    // Real `x + y` would be 42; the synthetic trace returns 9999.
    // A 9999 return value proves the dispatch loop was bypassed.
    assert_eq!(
        result,
        Value::Int(9999),
        "trace bypass must return the trace's result_slot, not the dispatch result"
    );
    assert_eq!(lookup.call_count(), 1);
}

/// `Deopt { snapshot }` outcome: the snapshot carries an
/// `external_pc` that routes to a real bytecode index. The resume
/// path picks up from there and finishes the computation. Verify by
/// constructing a snapshot pinned to the `Add` op of `x + y`; resume
/// should produce the real sum (the bytecode VM completes the Add).
#[test]
fn deopt_outcome_routes_through_resume_path() {
    use relon_bytecode::op::BcOp;

    let ev_template =
        BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y").expect("compile");
    let func = ev_template.function();
    let add_bc_idx = func
        .ops
        .iter()
        .position(|op| matches!(op, BcOp::AddI64 | BcOp::AddF64))
        .expect("Add present");
    let add_external_pc = func.ir_pc_map[add_bc_idx];

    // Build a snapshot at the Add op with an empty operand stack —
    // the recipe at that bc_idx is `[Local(0), Local(1)]`, so the
    // resume path rehydrates the stack from `args` directly.
    let snapshot = DeoptStateSnapshot::with_value_stack(
        0,
        add_external_pc,
        Vec::new().into_boxed_slice(),
        Vec::new().into_boxed_slice(),
    );
    let lookup = MockLookup::new(vec![TraceInvokeOutcome::Deopt {
        snapshot: Box::new(snapshot),
    }]);
    let handle: InstalledTraceLookupHandle = lookup.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(1003)
        .with_trace_lookup(handle);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let result = ev.run_main(args).expect("run_main");
    assert_eq!(
        result,
        Value::Int(42),
        "deopt resume from the Add bc_idx must complete the sum"
    );
    assert_eq!(lookup.call_count(), 1);
}

/// Without `fn_id`, the dispatcher switch stays inert — the lookup is
/// never consulted. Sanity-check the `func.fn_id.is_some()` gate.
#[test]
fn lookup_not_consulted_when_fn_id_missing() {
    let lookup = MockLookup::new(vec![TraceInvokeOutcome::Success { result: 9999 }]);
    let handle: InstalledTraceLookupHandle = lookup.clone();
    // `from_source` doesn't stamp a fn_id; without `.with_fn_id(...)`
    // the switch is inert.
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_trace_lookup(handle);
    let mut args = HashMap::new();
    args.insert("x".to_string(), Value::Int(40));
    args.insert("y".to_string(), Value::Int(2));
    let result = ev.run_main(args).expect("run_main");
    assert_eq!(result, Value::Int(42), "dispatch must run unchanged");
    assert_eq!(
        lookup.call_count(),
        0,
        "lookup must NOT be consulted when fn_id is absent"
    );
}

/// Repeat invocations: the lookup is consulted on every `run_main`.
/// We pre-load three responses; assert all three are popped and the
/// fall-through (`NoTrace`) on the 4th call still works.
#[test]
fn lookup_consulted_every_run_main() {
    let lookup = MockLookup::new(vec![
        TraceInvokeOutcome::Success { result: 100 },
        TraceInvokeOutcome::NoTrace,
        TraceInvokeOutcome::Success { result: 300 },
    ]);
    let handle: InstalledTraceLookupHandle = lookup.clone();
    let ev = BytecodeEvaluator::from_source("#main(Int x, Int y) -> Int\nx + y")
        .expect("compile")
        .with_fn_id(1004)
        .with_trace_lookup(handle);
    let mut a = HashMap::new();
    a.insert("x".to_string(), Value::Int(40));
    a.insert("y".to_string(), Value::Int(2));

    let r1 = ev.run_main(a.clone()).expect("r1");
    let r2 = ev.run_main(a.clone()).expect("r2");
    let r3 = ev.run_main(a.clone()).expect("r3");
    let r4 = ev.run_main(a.clone()).expect("r4"); // exhausted → NoTrace

    assert_eq!(r1, Value::Int(100), "first → trace returns 100");
    assert_eq!(r2, Value::Int(42), "second → NoTrace → dispatch yields 42");
    assert_eq!(r3, Value::Int(300), "third → trace returns 300");
    assert_eq!(r4, Value::Int(42), "fourth → exhausted → fallthrough");
    assert_eq!(lookup.call_count(), 4);
}
