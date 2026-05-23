//! Dead-store elimination smoke tests.

use relon_trace_jit::optimizer::dead_store::DeadStoreElim;
use relon_trace_jit::{
    EffectClass, ExternalPc, FuncId, GuardKind, GuardSite, ObservedType, Offset, OptimizerPass,
    TraceBuffer, TraceOp,
};

#[test]
fn dead_store_followed_by_overwrite_is_removed() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    });
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: c,
    });
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 1);
    // Surviving store is the second one (value `c`).
    let last_store = b.ops.iter().rev().find(|o| {
        matches!(
            o,
            TraceOp::Store {
                base: _,
                offset: _,
                src: _
            }
        )
    });
    assert!(
        matches!(last_store.unwrap(), TraceOp::Store { base: _, offset: _, src: s } if *s == c)
    );
}

#[test]
fn intervening_load_keeps_store_alive() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let loaded = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    });
    b.append(TraceOp::Load {
        dst: loaded,
        base,
        offset: Offset(0),
    });
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: c,
    });
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 0);
}

#[test]
fn intervening_guard_blocks_elimination() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    });
    b.append(TraceOp::Guard {
        kind: GuardKind::NotNull(base),
        check: base,
    });
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: c,
    });
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 0);
}

#[test]
fn different_offsets_are_independent() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    });
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(8),
        src: c,
    });
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 0);
}

#[test]
fn impure_call_blocks_elimination() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let cret = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    });
    b.append(TraceOp::Call {
        dst: cret,
        func: FuncId(1),
        args: vec![],
        effect: EffectClass::RecoverableWrite,
    });
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: c,
    });
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 0);
}

#[test]
fn guard_pcs_remapped_after_removal() {
    // Layout (pcs): 0 ConstI32 a
    //               1 Store base[0] = a     <- dead (no guard between)
    //               2 ConstI32 c
    //               3 Store base[0] = c     <- live
    //               4 Guard (anchored)
    // After elimination index 1 is dropped; the guard should slide
    // from pc 4 -> pc 3.
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    }); // dead
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: c,
    });
    b.append(TraceOp::Guard {
        kind: GuardKind::TypeCheck(c, ObservedType::I32),
        check: c,
    });
    b.record_guard(GuardSite::new(
        4,
        ExternalPc(0x10),
        GuardKind::TypeCheck(c, ObservedType::I32),
    ));
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 1);
    // Guard op slid from index 4 to index 3.
    assert_eq!(b.guards[0].trace_pc, 3);
    assert!(matches!(b.ops[3], TraceOp::Guard { kind: _, check: _ }));
}

#[test]
fn pure_call_between_stores_does_not_block() {
    let mut b = TraceBuffer::new();
    let base = b.fresh_ssa();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let cret = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    });
    b.append(TraceOp::Call {
        dst: cret,
        func: FuncId(1),
        args: vec![],
        effect: EffectClass::Pure,
    });
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: c,
    });
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 1);
}

#[test]
fn empty_trace_is_noop() {
    let mut b = TraceBuffer::new();
    let report = DeadStoreElim.run(&mut b);
    assert_eq!(report.ops_removed, 0);
    assert_eq!(b.op_count(), 0);
}
