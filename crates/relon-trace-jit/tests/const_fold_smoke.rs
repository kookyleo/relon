//! Constant-folding pass smoke tests.

use relon_trace_jit::optimizer::const_fold::ConstFold;
use relon_trace_jit::{CmpKind, Offset, OptimizerPass, SsaVar, TraceBuffer, TraceConst, TraceOp};

#[test]
fn fold_add_i32() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 3 });
    b.append(TraceOp::ConstI32 { dst: c, value: 4 });
    b.append(TraceOp::Add {
        dst,
        lhs: a,
        rhs: c,
    });
    let report = ConstFold.run(&mut b);
    assert_eq!(report.ops_replaced, 1);
    assert!(matches!(b.ops[2], TraceOp::ConstI32 { dst: d, value: 7 } if d == dst));
}

#[test]
fn fold_sub_mul_i64() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let d1 = b.fresh_ssa();
    let d2 = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 10 });
    b.append(TraceOp::ConstI64 { dst: c, value: 3 });
    b.append(TraceOp::Sub {
        dst: d1,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::Mul {
        dst: d2,
        lhs: a,
        rhs: c,
    });
    ConstFold.run(&mut b);
    assert!(matches!(b.ops[2], TraceOp::ConstI64 { dst: _, value: 7 }));
    assert!(matches!(b.ops[3], TraceOp::ConstI64 { dst: _, value: 30 }));
}

#[test]
fn fold_cmp_to_boolean_i32() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let d = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 5 });
    b.append(TraceOp::ConstI64 { dst: c, value: 3 });
    b.append(TraceOp::Cmp {
        kind: CmpKind::Gt,
        dst: d,
        lhs: a,
        rhs: c,
    });
    ConstFold.run(&mut b);
    assert!(matches!(b.ops[2], TraceOp::ConstI32 { dst: _, value: 1 }));
}

#[test]
fn unknown_input_leaves_op_unchanged() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let d = b.fresh_ssa();
    b.append(TraceOp::ConstI64 { dst: a, value: 5 });
    // c was never assigned a const.
    b.append(TraceOp::Add {
        dst: d,
        lhs: a,
        rhs: c,
    });
    let report = ConstFold.run(&mut b);
    assert_eq!(report.ops_replaced, 0);
    assert!(matches!(
        b.ops[1],
        TraceOp::Add {
            dst: _,
            lhs: _,
            rhs: _
        }
    ));
}

#[test]
fn chained_fold_collapses_in_one_pass() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let d = b.fresh_ssa();
    let e = b.fresh_ssa();
    let f = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 1 });
    b.append(TraceOp::ConstI32 { dst: c, value: 2 });
    b.append(TraceOp::Add {
        dst: d,
        lhs: a,
        rhs: c,
    });
    b.append(TraceOp::ConstI32 { dst: e, value: 3 });
    b.append(TraceOp::Mul {
        dst: f,
        lhs: d,
        rhs: e,
    });
    ConstFold.run(&mut b);
    // (1+2)*3 = 9
    assert!(matches!(b.ops[4], TraceOp::ConstI32 { dst: _, value: 9 }));
}

#[test]
fn store_acts_as_barrier_for_unrelated_chain() {
    // Stores never feed const ops here, so they should not block the
    // fold of an arith chain that does not depend on stored values.
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    let base = b.fresh_ssa();
    let d = b.fresh_ssa();
    b.append(TraceOp::ConstI32 { dst: a, value: 4 });
    b.append(TraceOp::ConstI32 { dst: c, value: 5 });
    b.append(TraceOp::Store {
        base,
        offset: Offset(0),
        src: a,
    });
    b.append(TraceOp::Add {
        dst: d,
        lhs: a,
        rhs: c,
    });
    ConstFold.run(&mut b);
    assert!(matches!(b.ops[3], TraceOp::ConstI32 { dst: _, value: 9 }));
}

#[test]
fn pre_existing_consts_table_seeds_fold() {
    let mut b = TraceBuffer::new();
    let a = SsaVar(0);
    let c = SsaVar(1);
    let dst = SsaVar(2);
    b.record_const(a, TraceConst::I32(10));
    b.record_const(c, TraceConst::I32(20));
    // No explicit ConstI32 ops in the buffer -- the seeds come from
    // the side table.
    b.append(TraceOp::Add {
        dst,
        lhs: a,
        rhs: c,
    });
    let r = ConstFold.run(&mut b);
    assert_eq!(r.ops_replaced, 1);
    assert!(matches!(b.ops[0], TraceOp::ConstI32 { dst: _, value: 30 }));
}
