//! Bytecode M3: closure dispatch (MakeClosure / CallClosure /
//! CaptureGet) smoke + correctness via hand-built `BcFunction`s.
//!
//! The IR-level closure lowering path (`Op::MakeClosure` /
//! `Op::CallClosure` -> the bytecode visitor) stays gated until the
//! parent-function-body hoist lands; until then the compile visitor
//! returns `UnsupportedOp` so source-level lambdas bounce through the
//! BackendError::Bytecode prong on `new_evaluator`. These tests
//! exercise the dispatch arms directly through hand-built
//! `BcFunction` instances — the same pattern the wider hand-built
//! corpus uses for ops the source-level lowering doesn't reach yet.

use relon_bytecode::{BcFunction, BcOp, BcVmConfig, BytecodeVm};

/// `(x: i64, y: i64) -> i64 { return x + y }` — a closure body
/// that consumes two positional args and returns their sum. Used
/// across multiple tests below as the canonical "callee body" shape.
fn closure_body_add_two_args() -> BcFunction {
    BcFunction {
        ops: vec![
            BcOp::LocalGet(0),
            BcOp::LocalGet(1),
            BcOp::AddI64,
            BcOp::Return,
        ],
        locals: 2,
        ir_pc_map: vec![0; 4],
        stack_recipe: vec![Vec::new(); 4],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
    }
}

/// `(x: i64) -> i64 { return capture0 + x }` — body that reads one
/// capture (`captures[0]`) and one positional argument, returns the
/// sum.
fn closure_body_one_capture_one_arg() -> BcFunction {
    BcFunction {
        ops: vec![
            BcOp::CaptureGet { idx: 0 },
            BcOp::LocalGet(0),
            BcOp::AddI64,
            BcOp::Return,
        ],
        locals: 1,
        ir_pc_map: vec![0; 4],
        stack_recipe: vec![Vec::new(); 4],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
    }
}

#[test]
fn make_closure_allocates_handle_and_records_captures() {
    // Outer body:
    //   const 7
    //   const 11
    //   make_closure body=0 captures=2  (pops 7,11 — captures=[7,11])
    //   return  (returns the closure handle as a u64)
    let body = closure_body_add_two_args();
    let outer = BcFunction {
        ops: vec![
            BcOp::ConstI64(7),
            BcOp::ConstI64(11),
            BcOp::MakeClosure {
                body_idx: 0,
                capture_count: 2,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![0; 4],
        stack_recipe: vec![Vec::new(); 4],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: vec![body],
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_none(), "vm error: {:?}", outcome.error);
    // The Return op pops the closure handle and pushes it as the
    // return value. The arena hands out monotonically-increasing u32
    // slots so the first allocation is `0`.
    assert_eq!(outcome.value, Some(0));
}

#[test]
fn call_closure_dispatches_through_body_and_returns_value() {
    // Outer body:
    //   make_closure body=0 captures=0
    //   const 100
    //   const 50
    //   call_closure argc=2  (body returns 100 + 50)
    //   return
    let body = closure_body_add_two_args();
    let outer = BcFunction {
        ops: vec![
            BcOp::MakeClosure {
                body_idx: 0,
                capture_count: 0,
            },
            BcOp::ConstI64(100),
            BcOp::ConstI64(50),
            BcOp::CallClosure { argc: 2 },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![0; 5],
        stack_recipe: vec![Vec::new(); 5],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: vec![body],
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_none(), "vm error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(150));
}

#[test]
fn capture_get_reads_closure_upvalue() {
    // Outer body:
    //   const 99                          ; capture0 value
    //   make_closure body=0 captures=1    ; pops 99 — captures=[99]
    //   const 1                           ; arg0 value
    //   call_closure argc=1               ; body returns capture0 + arg0 = 100
    //   return
    let body = closure_body_one_capture_one_arg();
    let outer = BcFunction {
        ops: vec![
            BcOp::ConstI64(99),
            BcOp::MakeClosure {
                body_idx: 0,
                capture_count: 1,
            },
            BcOp::ConstI64(1),
            BcOp::CallClosure { argc: 1 },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![0; 5],
        stack_recipe: vec![Vec::new(); 5],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: vec![body],
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_none(), "vm error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(100));
}

#[test]
fn closure_called_multiple_times_with_independent_args() {
    // Verify the captures vector is read-only across multiple
    // invocations — calling the same handle twice with different args
    // does not corrupt the captured state.
    let body = closure_body_one_capture_one_arg();
    let outer = BcFunction {
        ops: vec![
            // Capture value 1000.
            BcOp::ConstI64(1000),
            BcOp::MakeClosure {
                body_idx: 0,
                capture_count: 1,
            },
            // Stash the closure handle in local 0.
            BcOp::LocalSet(0),
            // First call: closure(7) -> 1007.
            BcOp::LocalGet(0),
            BcOp::ConstI64(7),
            BcOp::CallClosure { argc: 1 },
            // Stash the first result in local 1.
            BcOp::LocalSet(1),
            // Second call: closure(23) -> 1023.
            BcOp::LocalGet(0),
            BcOp::ConstI64(23),
            BcOp::CallClosure { argc: 1 },
            // Add the two results: 1007 + 1023 = 2030.
            BcOp::LocalGet(1),
            BcOp::AddI64,
            BcOp::Return,
        ],
        locals: 2,
        ir_pc_map: vec![0; 13],
        stack_recipe: vec![Vec::new(); 13],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: vec![body],
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_none(), "vm error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(2030));
}

#[test]
fn closure_body_with_internal_branch_is_dispatched_correctly() {
    // Body: `(x) -> if x < capture0 then capture0 else x`. Exercises
    // the dispatch_one branch ops + Jump targets inside the closure
    // body sub-loop.
    //
    // Op layout (cmp pops rhs first then lhs, so push LHS first then RHS):
    //   0: LocalGet 0        ; push x   (lhs)
    //   1: CaptureGet 0      ; push capture0 (rhs)
    //   2: Lt I64            ; x < capture0
    //   3: JumpIfFalse 6     ; if NOT less, branch to "return x"
    //   4: CaptureGet 0      ; push capture0
    //   5: Return            ; return capture0
    //   6: LocalGet 0        ; push x
    //   7: Return            ; return x
    let body = BcFunction {
        ops: vec![
            BcOp::LocalGet(0),
            BcOp::CaptureGet { idx: 0 },
            BcOp::LtI64,
            BcOp::JumpIfFalse(6),
            BcOp::CaptureGet { idx: 0 },
            BcOp::Return,
            BcOp::LocalGet(0),
            BcOp::Return,
        ],
        locals: 1,
        ir_pc_map: vec![0; 8],
        stack_recipe: vec![Vec::new(); 8],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
    };
    let outer = BcFunction {
        ops: vec![
            // capture0 = 50.
            BcOp::ConstI64(50),
            BcOp::MakeClosure {
                body_idx: 0,
                capture_count: 1,
            },
            BcOp::LocalSet(0),
            // closure(7) -> max(7, 50) = 50.
            BcOp::LocalGet(0),
            BcOp::ConstI64(7),
            BcOp::CallClosure { argc: 1 },
            BcOp::Return,
        ],
        locals: 1,
        ir_pc_map: vec![0; 7],
        stack_recipe: vec![Vec::new(); 7],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: vec![body],
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_none(), "vm error: {:?}", outcome.error);
    assert_eq!(outcome.value, Some(50));
}

#[test]
fn capture_get_outside_closure_traps() {
    // `BcOp::CaptureGet` emitted outside a closure body is a compiler
    // bug. The dispatch loop surfaces it as `StackUnderflow` (the
    // shared "compiler-bug" envelope) — no panic.
    let outer = BcFunction {
        ops: vec![BcOp::CaptureGet { idx: 0 }, BcOp::Return],
        locals: 0,
        ir_pc_map: vec![0; 2],
        stack_recipe: vec![Vec::new(); 2],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_some(), "outer CaptureGet should trap");
}

#[test]
fn closure_reducer_sum_of_zero_to_n_minus_one() {
    // Demonstrates the M3 closure surface end-to-end on a reducer-
    // shaped workload (the W1 "sum(range(n))" skeleton): the outer
    // function holds the loop and the closure encapsulates the
    // per-element step. While the source-level lowering for W1 isn't
    // landed yet, this test pins the dispatch invariants the future
    // pipeline will rely on:
    //
    //   - closure body called once per iteration with the loop index
    //   - captures read-only across the inner calls
    //   - return values flow through the outer accumulator local
    //   - per-call locals frame is isolated (no spill into outer)
    //
    // Pseudo-code:
    //   acc = 0
    //   i = 0
    //   step = (idx) => idx
    //   loop {
    //     if i >= 100 { break }
    //     acc += step(i)
    //     i += 1
    //   }
    //   return acc          ; expect 0+1+...+99 = 4950
    //
    // Local slots: 0=acc, 1=i, 2=step_handle.
    let step_body = BcFunction {
        ops: vec![BcOp::LocalGet(0), BcOp::Return],
        locals: 1,
        ir_pc_map: vec![0; 2],
        stack_recipe: vec![Vec::new(); 2],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
    };
    // Outer op layout:
    //   0: ConstI64 0          ; acc = 0
    //   1: LocalSet 0
    //   2: ConstI64 0          ; i = 0
    //   3: LocalSet 1
    //   4: MakeClosure body=0 captures=0
    //   5: LocalSet 2          ; step = closure
    //   --- loop header (idx 6) ---
    //   6: LocalGet 1          ; i
    //   7: ConstI64 100        ; n
    //   8: Ge I64              ; i >= n
    //   9: JumpIfTrue 21       ; exit on true
    //   10: LocalGet 0         ; acc
    //   11: LocalGet 2         ; step handle
    //   12: LocalGet 1         ; arg = i
    //   13: CallClosure argc=1 ; -> step(i)
    //   14: Add I64            ; acc += step(i)
    //   15: LocalSet 0         ; acc = ...
    //   16: LocalGet 1         ; i
    //   17: ConstI64 1
    //   18: Add I64
    //   19: LocalSet 1         ; i = i + 1
    //   20: Jump 6             ; loop back
    //   21: LocalGet 0         ; return acc
    //   22: Return
    let outer = BcFunction {
        ops: vec![
            BcOp::ConstI64(0),
            BcOp::LocalSet(0),
            BcOp::ConstI64(0),
            BcOp::LocalSet(1),
            BcOp::MakeClosure {
                body_idx: 0,
                capture_count: 0,
            },
            BcOp::LocalSet(2),
            BcOp::LocalGet(1),
            BcOp::ConstI64(100),
            BcOp::GeI64,
            BcOp::JumpIfTrue(21),
            BcOp::LocalGet(0),
            BcOp::LocalGet(2),
            BcOp::LocalGet(1),
            BcOp::CallClosure { argc: 1 },
            BcOp::AddI64,
            BcOp::LocalSet(0),
            BcOp::LocalGet(1),
            BcOp::ConstI64(1),
            BcOp::AddI64,
            BcOp::LocalSet(1),
            BcOp::Jump(6),
            BcOp::LocalGet(0),
            BcOp::Return,
        ],
        locals: 3,
        ir_pc_map: vec![0; 23],
        stack_recipe: vec![Vec::new(); 23],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: vec![step_body],
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_none(), "vm error: {:?}", outcome.error);
    // sum(0..100) = 99*100/2 = 4950.
    assert_eq!(outcome.value, Some(4950));
}

#[test]
fn call_closure_invalid_body_idx_traps() {
    // `MakeClosure { body_idx: 7 }` against an empty `closure_bodies`
    // slice surfaces as `StackUnderflow` (the same compiler-bug
    // envelope `CaptureGet` outside a closure uses). Validates the
    // bounds check inside the dispatch arm.
    let outer = BcFunction {
        ops: vec![
            BcOp::MakeClosure {
                body_idx: 7,
                capture_count: 0,
            },
            BcOp::Return,
        ],
        locals: 0,
        ir_pc_map: vec![0; 2],
        stack_recipe: vec![Vec::new(); 2],
        string_pool: Vec::new(),
        fn_id: None,
        closure_bodies: Vec::new(),
    };

    let vm = BytecodeVm::new(BcVmConfig::default());
    let outcome = vm.invoke(&outer, &[]);
    assert!(outcome.error.is_some(), "out-of-range body_idx should trap");
}
