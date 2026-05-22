//! M2-B phase 4c: integration tests for the bytecode VM hot-counter
//! prologue. Drives the dispatch loop against a mock `HotTraceTrigger`
//! that records every call, so the assertions can pin:
//!
//! - the counter bumps once per `invoke` against the same `fn_id`,
//! - the trigger fires exactly once on threshold crossing,
//! - subsequent invocations stay below the trigger (saturated slot),
//! - a `BcFunction` without `fn_id` leaves the prologue inert,
//! - a `BcVmConfig` without `hot_trigger` leaves the prologue inert,
//! - partial-resume entries (`start_bc_idx != 0`) skip the bump so the
//!   recorder doesn't get retriggered on every deopt bounce.

use std::sync::{Arc, Mutex};

use relon_bytecode::hot_counter::{peek_hot, reset_hot_all};
use relon_bytecode::op::{BcFunction, BcOp};
use relon_bytecode::vm::{BcVmConfig, BytecodeVm, VmValue};
use relon_bytecode::{HotTraceTrigger, HotTraceTriggerHandle, COUNTER_SATURATED};

/// Test mock that pushes every (fn_id, args) trigger event into a
/// shared `Vec` for the assertions to inspect.
struct MockTrigger {
    log: Mutex<Vec<(u32, Vec<VmValue>)>>,
}

impl MockTrigger {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(u32, Vec<VmValue>)> {
        self.log.lock().unwrap().clone()
    }
}

impl HotTraceTrigger for MockTrigger {
    fn on_hot(&self, fn_id: u32, args: &[VmValue]) {
        self.log.lock().unwrap().push((fn_id, args.to_vec()));
    }
}

/// Minimal hot-loop-shaped BcFunction: read arg 0, return it. The body
/// is short enough that each `invoke` cleanly maps to "one iteration"
/// of the corpus's hot loop without dragging stdlib / list / dict ops
/// (those need the buffer-protocol envelope phase 4 still parks).
fn make_id_function(fn_id: u32) -> BcFunction {
    BcFunction {
        ops: vec![BcOp::LocalGet(0), BcOp::Return],
        locals: 1,
        ir_pc_map: vec![1, 2],
        stack_recipe: vec![vec![], vec![]],
        string_pool: Vec::new(),
        fn_id: Some(fn_id),
        closure_bodies: Vec::new(),
        requires_cap_consult: false,
    }
}

/// Same shape, but with `fn_id: None` — the hot-counter prologue
/// should stay inert no matter how many times we invoke.
fn make_id_function_no_id() -> BcFunction {
    BcFunction {
        ops: vec![BcOp::LocalGet(0), BcOp::Return],
        locals: 1,
        ir_pc_map: vec![1, 2],
        stack_recipe: vec![vec![], vec![]],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
        requires_cap_consult: false,
    }
}

#[test]
fn hot_counter_fires_trigger_at_threshold() {
    reset_hot_all();
    let trigger = MockTrigger::new();
    let handle: HotTraceTriggerHandle = trigger.clone();
    let cfg = BcVmConfig {
        hot_trigger: Some(handle),
        hot_threshold: 3,
        ..Default::default()
    };
    let vm = BytecodeVm::new(cfg);
    let func = make_id_function(/*fn_id=*/ 42);

    // 1st invoke: Cold — counter goes to 1.
    let _ = vm.invoke(&func, &[7]);
    assert_eq!(peek_hot(42), Some(1));
    assert!(
        trigger.calls().is_empty(),
        "trigger must not fire below threshold"
    );

    // 2nd invoke: Heating(2).
    let _ = vm.invoke(&func, &[7]);
    assert_eq!(peek_hot(42), Some(2));
    assert!(trigger.calls().is_empty());

    // 3rd invoke: HotTrigger — slot saturates, trigger fires once.
    let out = vm.invoke(&func, &[7]);
    assert_eq!(out.value, Some(7), "the VM still returns the real value");
    assert_eq!(peek_hot(42), Some(COUNTER_SATURATED));
    let calls = trigger.calls();
    assert_eq!(calls.len(), 1, "trigger fires exactly once at threshold");
    assert_eq!(calls[0].0, 42);
    assert_eq!(calls[0].1, vec![7]);

    // 4th invoke: AlreadyHot — slot stays saturated, trigger does NOT
    // re-fire (the host already drove a recording attempt; reruns are
    // the install-pipeline's responsibility).
    let _ = vm.invoke(&func, &[7]);
    assert_eq!(peek_hot(42), Some(COUNTER_SATURATED));
    assert_eq!(trigger.calls().len(), 1, "trigger does not re-fire");
}

#[test]
fn hot_counter_inert_without_trigger() {
    reset_hot_all();
    // Default config has no `hot_trigger` installed — the prologue
    // must stay inert.
    let vm = BytecodeVm::new(BcVmConfig::default());
    let func = make_id_function(/*fn_id=*/ 99);
    for _ in 0..5 {
        let _ = vm.invoke(&func, &[3]);
    }
    // No trigger means no counter touch (the prologue short-circuits
    // before even calling `record_hot`).
    assert_eq!(peek_hot(99), None);
}

#[test]
fn hot_counter_inert_without_fn_id() {
    reset_hot_all();
    let trigger = MockTrigger::new();
    let handle: HotTraceTriggerHandle = trigger.clone();
    let cfg = BcVmConfig {
        hot_trigger: Some(handle),
        hot_threshold: 2,
        ..Default::default()
    };
    let vm = BytecodeVm::new(cfg);
    let func = make_id_function_no_id();
    for _ in 0..10 {
        let _ = vm.invoke(&func, &[1]);
    }
    assert!(
        trigger.calls().is_empty(),
        "no fn_id means no hot-counter wire-up"
    );
}

#[test]
fn partial_resume_skips_hot_counter_bump() {
    reset_hot_all();
    let trigger = MockTrigger::new();
    let handle: HotTraceTriggerHandle = trigger.clone();
    let cfg = BcVmConfig {
        hot_trigger: Some(handle),
        hot_threshold: 1,
        ..Default::default()
    };
    let vm = BytecodeVm::new(cfg);
    let func = make_id_function(/*fn_id=*/ 5);

    // Pretend the trace-JIT deopted us into the middle of the body.
    // The hot-counter prologue must NOT bump on partial-resume entries
    // (start_bc_idx != 0) — otherwise every deopt would retrigger the
    // recorder and the install pipeline would thrash.
    //
    // We invoke at start_bc_idx=1 with the operand-stack pre-seeded
    // (the `Return` op pops one slot, so a `[42]` seed makes the run
    // complete cleanly with value=42).
    let outcome = vm.invoke_from_with_stack(
        &func,
        &[7],
        /*start_bc_idx=*/ 1,
        /*extra_locals=*/ &[],
        /*return_slot_count=*/ 0,
        /*initial_stack=*/ &[42],
    );
    assert_eq!(outcome.value, Some(42));
    assert!(
        trigger.calls().is_empty(),
        "partial-resume entry must not bump the hot counter"
    );
    assert_eq!(peek_hot(5), None);
}

#[test]
fn distinct_fn_ids_track_independently() {
    reset_hot_all();
    let trigger = MockTrigger::new();
    let handle: HotTraceTriggerHandle = trigger.clone();
    let cfg = BcVmConfig {
        hot_trigger: Some(handle),
        hot_threshold: 2,
        ..Default::default()
    };
    let vm = BytecodeVm::new(cfg);
    let func_a = make_id_function(/*fn_id=*/ 100);
    let func_b = make_id_function(/*fn_id=*/ 200);

    // Interleave invocations: A, B, A, B. Each fn_id reaches its
    // threshold independently — neither pollutes the other's slot.
    let _ = vm.invoke(&func_a, &[1]);
    let _ = vm.invoke(&func_b, &[2]);
    assert_eq!(peek_hot(100), Some(1));
    assert_eq!(peek_hot(200), Some(1));
    assert!(trigger.calls().is_empty());

    // Second round: both saturate.
    let _ = vm.invoke(&func_a, &[1]);
    let _ = vm.invoke(&func_b, &[2]);
    assert_eq!(peek_hot(100), Some(COUNTER_SATURATED));
    assert_eq!(peek_hot(200), Some(COUNTER_SATURATED));
    let calls = trigger.calls();
    assert_eq!(calls.len(), 2);
    // Order: A's second invoke fires first, then B's.
    assert_eq!(calls[0].0, 100);
    assert_eq!(calls[0].1, vec![1]);
    assert_eq!(calls[1].0, 200);
    assert_eq!(calls[1].1, vec![2]);
}

#[test]
fn args_passed_to_trigger_match_invoke_call() {
    reset_hot_all();
    let trigger = MockTrigger::new();
    let handle: HotTraceTriggerHandle = trigger.clone();
    let cfg = BcVmConfig {
        hot_trigger: Some(handle),
        hot_threshold: 1,
        ..Default::default()
    };
    let vm = BytecodeVm::new(cfg);
    // Build a function with two arg slots.
    let func = BcFunction {
        ops: vec![
            BcOp::LocalGet(0),
            BcOp::LocalGet(1),
            BcOp::AddI64,
            BcOp::Return,
        ],
        locals: 2,
        ir_pc_map: vec![1, 2, 3, 4],
        stack_recipe: vec![vec![], vec![], vec![], vec![]],
        string_pool: Vec::new(),
        fn_id: Some(7),
        closure_bodies: Vec::new(),
        requires_cap_consult: false,
    };
    let out = vm.invoke(&func, &[10, 32]);
    assert_eq!(out.value, Some(42));
    let calls = trigger.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], (7, vec![10, 32]));
}
