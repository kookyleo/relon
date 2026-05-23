//! Load-forwarding pass smoke tests.

use relon_trace_jit::optimizer::dead_store::DeadStoreElim;
use relon_trace_jit::optimizer::load_forward::LoadForwarding;
use relon_trace_jit::{
    EffectClass, FuncId, GuardKind, ObservedType, Offset, OptimizerPass, SsaVar, TraceBuffer,
    TraceOp,
};

/// Pull the SSA inputs of a load op for assertion convenience.
fn load_dst(op: &TraceOp) -> Option<SsaVar> {
    op.output()
}

#[test]
fn basic_store_load_forwards_value() {
    // Store base:off=0 = v1
    // Load dst, base:off=0
    // Add res, dst, v1   ->  Add res, v1, v1
    let mut b = TraceBuffer::new();
    let v1 = b.fresh_ssa();
    let base = b.fresh_ssa();
    let dst = b.fresh_ssa();
    let res = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v1, value: 7 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v1,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Add {
        dst: res,
        lhs: dst,
        rhs: v1,
    });

    let r = LoadForwarding.run(&mut b);
    // Add op should now read (v1, v1).
    let add = &b.ops[3];
    match add {
        TraceOp::Add {
            dst: _,
            lhs: a,
            rhs: c,
        } => {
            assert_eq!(*a, v1);
            assert_eq!(*c, v1);
        }
        other => panic!("expected Add, got {other:?}"),
    }
    assert!(r.ops_replaced >= 1);
    // Load op stays in place -- DSE elim drops it on the next pipeline run.
    assert_eq!(load_dst(&b.ops[2]), Some(dst));
}

#[test]
fn intermediate_pure_op_does_not_break_alias() {
    let mut b = TraceBuffer::new();
    let v1 = b.fresh_ssa();
    let base = b.fresh_ssa();
    let pure_d = b.fresh_ssa();
    let dst = b.fresh_ssa();
    let final_d = b.fresh_ssa();
    b.append(TraceOp::ConstI64 {
        dst: v1,
        value: 100,
    });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v1,
    });
    // A Pure op between Store and Load -- must not invalidate.
    b.append(TraceOp::Add {
        dst: pure_d,
        lhs: v1,
        rhs: v1,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Sub {
        dst: final_d,
        lhs: dst,
        rhs: v1,
    });

    LoadForwarding.run(&mut b);
    let sub = &b.ops[4];
    match sub {
        TraceOp::Sub {
            dst: _,
            lhs: a,
            rhs: c,
        } => {
            assert_eq!(*a, v1, "load should have been forwarded across Pure op");
            assert_eq!(*c, v1);
        }
        other => panic!("expected Sub, got {other:?}"),
    }
}

#[test]
fn recoverable_call_flushes_alias_table() {
    // Store base, off=0 = v1
    // Call with RecoverableWrite -- flushes slot table.
    // Load dst, base, off=0 -- must NOT forward.
    let mut b = TraceBuffer::new();
    let v1 = b.fresh_ssa();
    let base = b.fresh_ssa();
    let call_ret = b.fresh_ssa();
    let dst = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v1, value: 5 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v1,
    });
    b.append(TraceOp::Call {
        dst: call_ret,
        func: FuncId(9),
        args: vec![],
        effect: EffectClass::RecoverableWrite,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Add {
        dst: r,
        lhs: dst,
        rhs: v1,
    });

    LoadForwarding.run(&mut b);
    // The Add op must still reference dst (not v1) -- the flush
    // prevented forwarding.
    let add = &b.ops[4];
    match add {
        TraceOp::Add {
            dst: _,
            lhs: a,
            rhs: _,
        } => assert_eq!(
            *a, dst,
            "forwarding must be blocked by RecoverableWrite call"
        ),
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn distinct_addresses_do_not_invalidate_each_other() {
    // Store at (base, 0) = v_a
    // Store at (base, 8) = v_b
    // Load at (base, 0)   -- should forward v_a
    // Load at (base, 8)   -- should forward v_b
    let mut b = TraceBuffer::new();
    let v_a = b.fresh_ssa();
    let v_b = b.fresh_ssa();
    let base = b.fresh_ssa();
    let d0 = b.fresh_ssa();
    let d8 = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v_a, value: 1 });
    b.append(TraceOp::ConstI64 { dst: v_b, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v_a,
    });
    b.append(TraceOp::Store {
        base,
        offset: Offset(8),
        src: v_b,
    });
    b.append(TraceOp::Load {
        dst: d0,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Load {
        dst: d8,
        base,
        offset: Offset(8),
    });
    b.append(TraceOp::Add {
        dst: r,
        lhs: d0,
        rhs: d8,
    });

    LoadForwarding.run(&mut b);
    match &b.ops[6] {
        TraceOp::Add {
            dst: _,
            lhs: a,
            rhs: c,
        } => {
            assert_eq!(*a, v_a, "first load should forward v_a");
            assert_eq!(*c, v_b, "second load should forward v_b");
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

#[test]
fn alias_chain_collapses_through_multiple_loads() {
    // Store base:0 = v1
    // Load dst1, base:0      -> dst1 aliased to v1
    // Store base:8 = dst1    (effectively storing v1)
    // Load dst2, base:8      -> dst2 aliased to v1 (chain collapse)
    // Return dst2            -> Return v1
    let mut b = TraceBuffer::new();
    let v1 = b.fresh_ssa();
    let base = b.fresh_ssa();
    let dst1 = b.fresh_ssa();
    let dst2 = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v1, value: 42 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v1,
    });
    b.append(TraceOp::Load {
        dst: dst1,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Store {
        base,
        offset: Offset(8),
        src: dst1,
    });
    b.append(TraceOp::Load {
        dst: dst2,
        base,
        offset: Offset(8),
    });
    b.append(TraceOp::Return { value: dst2 });

    LoadForwarding.run(&mut b);
    // The Return op should now read v1, not dst2.
    match &b.ops[5] {
        TraceOp::Return { value: v } => assert_eq!(*v, v1, "alias chain should collapse"),
        other => panic!("expected Return, got {other:?}"),
    }
}

#[test]
fn dead_store_after_forward_drops_load() {
    // Pair load_forward with dead_store -- verify the Load op
    // becomes dead and is removed by the next DSE round.
    let mut b = TraceBuffer::new();
    let v1 = b.fresh_ssa();
    let base = b.fresh_ssa();
    let dst = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v1, value: 99 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v1,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Add {
        dst: r,
        lhs: dst,
        rhs: v1,
    });

    LoadForwarding.run(&mut b);
    // After forwarding, the Load is dead. DSE doesn't actually
    // drop Load ops today (it only drops redundant Stores), so
    // we assert at minimum that the forwarded Add no longer
    // references dst.
    match &b.ops[3] {
        TraceOp::Add {
            dst: _,
            lhs: a,
            rhs: c,
        } => {
            assert_ne!(*a, dst);
            assert_ne!(*c, dst);
        }
        other => panic!("expected Add, got {other:?}"),
    }
    // The Load op still sits at index 2, but its dst is no longer
    // used downstream.
    assert!(matches!(
        b.ops[2],
        TraceOp::Load {
            dst: _,
            base: _,
            offset: _
        }
    ));
    // DSE is safe to run; report should be empty since the only
    // dead-by-forwarding op is a Load, not a Store.
    let dse_report = DeadStoreElim.run(&mut b);
    assert_eq!(dse_report.ops_removed, 0);
}

#[test]
fn store_overwrite_uses_latest_value() {
    // Two stores to same slot; the load reads the second one.
    let mut b = TraceBuffer::new();
    let v1 = b.fresh_ssa();
    let v2 = b.fresh_ssa();
    let base = b.fresh_ssa();
    let dst = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v1, value: 1 });
    b.append(TraceOp::ConstI64 { dst: v2, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v1,
    });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v2,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Sub {
        dst: r,
        lhs: dst,
        rhs: v1,
    });

    LoadForwarding.run(&mut b);
    match &b.ops[5] {
        TraceOp::Sub {
            dst: _,
            lhs: a,
            rhs: _,
        } => {
            assert_eq!(*a, v2, "latest store should be forwarded");
        }
        other => panic!("expected Sub, got {other:?}"),
    }
}

#[test]
fn guard_does_not_invalidate_alias_table() {
    // A guard op between Store and Load should NOT block forwarding
    // (a guard's effect class is Pure).
    let mut b = TraceBuffer::new();
    let v1 = b.fresh_ssa();
    let base = b.fresh_ssa();
    let dst = b.fresh_ssa();
    let r = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: v1, value: 17 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: v1,
    });
    b.append(TraceOp::Guard {
        kind: GuardKind::TypeCheck(v1, ObservedType::I64),
        check: v1,
    });
    b.append(TraceOp::Load {
        dst,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::Add {
        dst: r,
        lhs: dst,
        rhs: v1,
    });

    LoadForwarding.run(&mut b);
    match &b.ops[4] {
        TraceOp::Add {
            dst: _,
            lhs: a,
            rhs: _,
        } => assert_eq!(*a, v1, "guard should not block forwarding"),
        other => panic!("expected Add, got {other:?}"),
    }
}
