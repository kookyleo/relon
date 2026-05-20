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

// ---- F-D8-E.3: ListGet / DictLookup / BoundsCheck hoist ------------

#[test]
fn loop_invariant_list_get_lifts_out_of_loop() {
    // Layout (e.g. `xs[0]` with constant 0 index — pathological but
    // realistic when the recorder propagated a const fold):
    //   ConstI64 list_ptr, ...   (pre-loop, opaque ptr)
    //   ConstI64 idx, 0          (pre-loop)
    //   MarkLoopHead 0
    //   Guard(BoundsCheck(idx, list_ptr))   <- loop-invariant
    //   ListGet { dst, list_ptr, idx }      <- loop-invariant
    //   MarkLoopBack 0
    //
    // After LICM both the guard and the ListGet should sit before
    // MarkLoopHead.
    let mut b = TraceBuffer::new();
    let list_ptr = b.fresh_ssa();
    let idx = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(list_ptr, 0x1000));
    b.append(TraceOp::ConstI64(idx, 0));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Guard(GuardKind::BoundsCheck(idx, list_ptr), idx));
    b.append(TraceOp::ListGet { dst, list_ptr, idx });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    let r = LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).expect("head missing");
    let guard_pos = position(&b.ops, |o| {
        matches!(o, TraceOp::Guard(GuardKind::BoundsCheck(_, _), _))
    })
    .expect("BoundsCheck guard missing");
    let list_get_pos =
        position(&b.ops, |o| matches!(o, TraceOp::ListGet { .. })).expect("ListGet missing");
    assert!(
        guard_pos < head_idx,
        "BoundsCheck guard must be hoisted before MarkLoopHead, found at {guard_pos} vs head {head_idx}"
    );
    assert!(
        list_get_pos < head_idx,
        "ListGet must be hoisted before MarkLoopHead"
    );
    // Guard precedes the load so the deopt anchor stays adjacent.
    assert!(
        guard_pos < list_get_pos,
        "BoundsCheck guard must remain immediately before its ListGet"
    );
    assert!(r.ops_replaced >= 2);
}

#[test]
fn loop_variant_idx_keeps_list_get_inside() {
    // The idx is produced inside the loop body — only `list_ptr` is
    // invariant. ListGet (and its BoundsCheck guard) must stay inside.
    let mut b = TraceBuffer::new();
    let list_ptr = b.fresh_ssa();
    let counter_base = b.fresh_ssa();
    let idx_inside = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(list_ptr, 0x1000));
    b.append(TraceOp::ConstI64(counter_base, 0));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    // idx_inside = Load(counter_base, 0) — synthesises a loop-variant
    // SSA without dragging in MarkLoopHead φ wiring.
    b.append(TraceOp::Load(idx_inside, counter_base, Offset(0)));
    b.append(TraceOp::Guard(
        GuardKind::BoundsCheck(idx_inside, list_ptr),
        idx_inside,
    ));
    b.append(TraceOp::ListGet {
        dst,
        list_ptr,
        idx: idx_inside,
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let list_get_pos = position(&b.ops, |o| matches!(o, TraceOp::ListGet { .. })).unwrap();
    let guard_pos = position(&b.ops, |o| {
        matches!(o, TraceOp::Guard(GuardKind::BoundsCheck(_, _), _))
    })
    .unwrap();
    assert!(
        list_get_pos > head_idx,
        "ListGet with loop-variant idx must stay inside the body"
    );
    assert!(
        guard_pos > head_idx,
        "BoundsCheck on a loop-variant idx must stay inside the body"
    );
}

#[test]
fn loop_invariant_dict_lookup_lifts_out_of_loop() {
    // Layout (e.g. `d["k"]` where both pointers come from outside):
    //   ConstI64 dict_ptr, ...   (pre-loop)
    //   ConstI64 key_ptr, ...    (pre-loop)
    //   MarkLoopHead 0
    //   DictLookup { dst, dict_ptr, key_ptr, shape_hash }
    //   MarkLoopBack 0
    let mut b = TraceBuffer::new();
    let dict_ptr = b.fresh_ssa();
    let key_ptr = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(dict_ptr, 0x2000));
    b.append(TraceOp::ConstI64(key_ptr, 0x3000));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::DictLookup {
        dst,
        dict_ptr,
        key_ptr,
        shape_hash: 0xdead_beef,
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    let r = LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let dict_pos =
        position(&b.ops, |o| matches!(o, TraceOp::DictLookup { .. })).expect("DictLookup missing");
    assert!(
        dict_pos < head_idx,
        "Loop-invariant DictLookup must hoist before MarkLoopHead"
    );
    assert!(r.ops_replaced >= 1);
}

#[test]
fn loop_variant_key_keeps_dict_lookup_inside() {
    // dict_ptr is invariant; key_ptr is loop-internal — DictLookup
    // must stay inside the body.
    let mut b = TraceBuffer::new();
    let dict_ptr = b.fresh_ssa();
    let key_base = b.fresh_ssa();
    let key_ptr_inside = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(dict_ptr, 0x2000));
    b.append(TraceOp::ConstI64(key_base, 0));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Load(key_ptr_inside, key_base, Offset(0)));
    b.append(TraceOp::DictLookup {
        dst,
        dict_ptr,
        key_ptr: key_ptr_inside,
        shape_hash: 0xcafe_babe,
    });
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let dict_pos = position(&b.ops, |o| matches!(o, TraceOp::DictLookup { .. })).unwrap();
    assert!(
        dict_pos > head_idx,
        "DictLookup with loop-variant key_ptr must stay inside the body"
    );
}

#[test]
fn loop_variant_bounds_check_stays_inside() {
    // A `BoundsCheck` whose idx is loop-variant must NOT be hoisted —
    // its pass/fail decision changes per iteration. (The matching
    // `ListGet` is exercised elsewhere; this test isolates the guard.)
    let mut b = TraceBuffer::new();
    let list_ptr = b.fresh_ssa();
    let counter_base = b.fresh_ssa();
    let idx = b.fresh_ssa();
    b.append(TraceOp::ConstI64(list_ptr, 0x1000));
    b.append(TraceOp::ConstI64(counter_base, 0));
    b.append(TraceOp::MarkLoopHead {
        loop_id: 0,
        phis: vec![],
    });
    b.append(TraceOp::Load(idx, counter_base, Offset(0)));
    b.append(TraceOp::Guard(GuardKind::BoundsCheck(idx, list_ptr), idx));
    b.append(TraceOp::MarkLoopBack {
        loop_id: 0,
        next_values: vec![],
    });

    LICM.run(&mut b);
    let head_idx = position(&b.ops, |o| o.is_loop_head()).unwrap();
    let guard_pos = position(&b.ops, |o| {
        matches!(o, TraceOp::Guard(GuardKind::BoundsCheck(_, _), _))
    })
    .unwrap();
    assert!(
        guard_pos > head_idx,
        "BoundsCheck on loop-variant idx must remain inside the body"
    );
}

#[test]
fn non_bounds_guards_remain_pinned_even_when_invariant() {
    // F-D8-E.3 only opens the gate for `BoundsCheck`. A `TypeCheck`
    // whose input is loop-invariant must still stay where the
    // recorder pinned it, matching the doc-stated position-sensitive
    // semantics for non-bounds guards.
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
    let guard_pos = position(&b.ops, |o| o.is_guard()).unwrap();
    assert!(
        guard_pos > head_idx,
        "Non-BoundsCheck guards stay pinned even when their input is invariant"
    );
}
