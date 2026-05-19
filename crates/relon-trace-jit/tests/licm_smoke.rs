//! LICM (loop invariant code motion) smoke tests.

use relon_trace_jit::optimizer::licm::LICM;
use relon_trace_jit::{
    EffectClass, FuncId, GuardKind, ObservedType, Offset, OptimizerPass, SsaVar, TraceBuffer,
    TraceOp,
};

/// Find the position of an op matching `pred`. Used to assert that a
/// hoisted op sits before a marker.
fn position<F: Fn(&TraceOp) -> bool>(ops: &[TraceOp], pred: F) -> Option<usize> {
    ops.iter().position(pred)
}

#[test]
fn simple_hoistable_add_lifts_out_of_loop() {
    // Loop layout:
    //   ConstI32 a, 3           (pre-loop)
    //   ConstI32 b, 4           (pre-loop)
    //   MarkLoopHead 0
    //   Add c, a, b             (loop-invariant -- should hoist)
    //   MarkLoopBack 0
    //
    // After LICM the Add should sit *before* MarkLoopHead.
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let bb = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32(a, 3));
    b.append(TraceOp::ConstI32(bb, 4));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Add(c, a, bb));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    let r = LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).expect("loop head missing");
    let add_idx = position(&b.ops, |o| matches!(o, TraceOp::Add(_, _, _))).expect("add missing");
    assert!(
        add_idx < head_idx,
        "Add must be hoisted before MarkLoopHead"
    );
    assert!(r.ops_replaced >= 1);
}

#[test]
fn loop_variant_op_stays_inside() {
    // The Add depends on a loop-internal Load output -> not invariant.
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let pre_c = b.fresh_ssa();
    let inside_load = b.fresh_ssa();
    let res = b.fresh_ssa();
    b.append(TraceOp::ConstI32(pre_c, 5));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Load(inside_load, base, Offset(0)));
    b.append(TraceOp::Add(res, inside_load, pre_c));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let add_idx = position(&b.ops, |o| matches!(o, TraceOp::Add(_, _, _))).unwrap();
    assert!(
        add_idx > head_idx,
        "Add depends on Load (loop-defined) and must stay inside the body"
    );
}

#[test]
fn recoverable_write_is_not_hoisted() {
    // A Store inside the loop is RecoverableWrite -- never hoisted.
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let val = b.fresh_ssa();
    b.append(TraceOp::ConstI64(val, 1));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Store(base, Offset(0), val));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let store_idx = position(&b.ops, |o| matches!(o, TraceOp::Store(_, _, _))).unwrap();
    assert!(
        store_idx > head_idx,
        "Store (RecoverableWrite) must not be hoisted"
    );
}

#[test]
fn guard_is_not_hoisted() {
    // A Guard inside the loop must stay where it was, even if its
    // operand is loop-invariant.
    let mut b = TraceBuffer::new();
    let v = b.fresh_ssa();
    b.append(TraceOp::ConstI64(v, 7));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Guard(
        GuardKind::TypeCheck(v, ObservedType::I64),
        v,
    ));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let guard_idx = position(&b.ops, |o| o.is_guard()).unwrap();
    assert!(guard_idx > head_idx, "Guard must remain inside the loop");
}

#[test]
fn nested_loops_lift_innermost_first() {
    // Layout:
    //   ConstI32 a, 1     (pre-outer)
    //   MarkLoopHead 0    (outer)
    //   MarkLoopHead 1    (inner)
    //   Mul c, a, a       (invariant in both loops)
    //   MarkLoopBack 1
    //   MarkLoopBack 0
    //
    // After LICM, the Mul should bubble all the way out to before
    // MarkLoopHead 0 (since the pass repeatedly lifts on each
    // iteration).
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32(a, 1));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::MarkLoopHead {
        loop_id: 1,
        phis: vec![],
    });
    b.append(TraceOp::Mul(c, a, a));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 1,
        next_values: vec![],
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let outer_head = b
        .ops
        .iter()
        .position(|o| o.loop_head_id() == Some(0))
        .unwrap();
    let mul_idx = position(&b.ops, |o| matches!(o, TraceOp::Mul(_, _, _))).unwrap();
    assert!(
        mul_idx < outer_head,
        "Mul should bubble out past the outermost MarkLoopHead"
    );
}

#[test]
fn nested_loops_partial_invariant_hoists_to_inner_head_only() {
    // Layout:
    //   ConstI32 a, 1            (pre-outer)
    //   MarkLoopHead 0           (outer)
    //   Load b, base, 0          (varies between outer iterations)
    //   MarkLoopHead 1           (inner)
    //   Add c, a, b              (invariant for inner only)
    //   MarkLoopBack 1
    //   MarkLoopBack 0
    //
    // After LICM, Add should sit between outer-head and inner-head,
    // not before the outer-head.
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let base = b.fresh_ssa();
    let bb = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32(a, 1));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Load(bb, base, Offset(0)));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 1,
        phis: vec![],
    });
    b.append(TraceOp::Add(c, a, bb));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 1,
        next_values: vec![],
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let outer_head = b
        .ops
        .iter()
        .position(|o| o.loop_head_id() == Some(0))
        .unwrap();
    let inner_head = b
        .ops
        .iter()
        .position(|o| o.loop_head_id() == Some(1))
        .unwrap();
    let add_idx = position(&b.ops, |o| matches!(o, TraceOp::Add(_, _, _))).unwrap();
    assert!(
        add_idx > outer_head,
        "Add depends on Load (outer-loop-defined), cannot leave outer loop"
    );
    assert!(
        add_idx < inner_head,
        "Add should still be hoisted out of the inner loop"
    );
}

#[test]
fn unrelated_loop_with_no_invariants_leaves_trace_unchanged() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let inside_load = b.fresh_ssa();
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Load(inside_load, base, Offset(0)));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });
    let before = b.ops.len();
    let r = LICM.run(&mut b);
    assert_eq!(b.ops.len(), before, "no-op LICM must not shrink trace");
    assert_eq!(r.ops_replaced, 0);
}

#[test]
fn pure_call_with_external_args_is_hoistable() {
    // A Call with EffectClass::Pure whose args are external should
    // be hoisted -- callers can declare arithmetic helpers Pure.
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.append(TraceOp::ConstI32(arg, 5));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Call(ret, FuncId(11), vec![arg], EffectClass::Pure));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });
    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let call_idx = position(&b.ops, |o| matches!(o, TraceOp::Call(_, _, _, _))).unwrap();
    assert!(call_idx < head_idx, "Pure Call should be hoistable");
}

#[test]
fn readonly_call_is_not_hoisted() {
    // ReadOnly is conservatively excluded from hoisting.
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.append(TraceOp::ConstI32(arg, 5));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Call(
        ret,
        FuncId(11),
        vec![arg],
        EffectClass::ReadOnly,
    ));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });
    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let call_idx = position(&b.ops, |o| matches!(o, TraceOp::Call(_, _, _, _))).unwrap();
    assert!(call_idx > head_idx, "ReadOnly Call must not be hoisted");
}

#[test]
fn ssa_var_passthrough_assertions_compile() {
    // Sanity: SsaVar(0) is a stable constant we can assert against
    // -- guards against future renames.
    let _v = SsaVar(0);
}
