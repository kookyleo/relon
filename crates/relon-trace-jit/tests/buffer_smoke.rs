//! TraceBuffer smoke tests.

use relon_trace_jit::{
    EffectClass, FuncId, GuardKind, GuardSite, ObservedType, Offset, OptimizerPipeline,
    SerializableSideTables, SsaVar, TraceBuffer, TraceConst, TraceOp,
};

#[test]
fn append_and_count() {
    let mut b = TraceBuffer::new();
    let a = b.fresh_ssa();
    let c = b.fresh_ssa();
    b.append(TraceOp::ConstI32(a, 1));
    b.append(TraceOp::ConstI32(c, 2));
    assert_eq!(b.op_count(), 2);
}

#[test]
fn fresh_ssa_advances_past_manual_dst() {
    let mut b = TraceBuffer::new();
    let manual = SsaVar(99);
    b.append(TraceOp::ConstI64(manual, 7));
    let next = b.fresh_ssa();
    assert_eq!(next, SsaVar(100));
}

#[test]
fn record_guard_stores_metadata() {
    let mut b = TraceBuffer::new();
    let var = b.fresh_ssa();
    b.append(TraceOp::ConstI64(var, 0));
    b.record_guard(GuardSite::new(
        0,
        relon_trace_jit::ExternalPc(0xdead),
        GuardKind::TypeCheck(var, ObservedType::I64),
    ));
    assert_eq!(b.guard_count(), 1);
}

#[test]
fn into_optimized_carries_state() {
    let mut b = TraceBuffer::new();
    let x = b.fresh_ssa();
    b.record_type(x, ObservedType::I32);
    b.record_const(x, TraceConst::I32(99));
    b.append(TraceOp::ConstI32(x, 99));
    let opt = b.into_optimized();
    assert_eq!(opt.op_count(), 1);
    assert_eq!(opt.type_info[&x], ObservedType::I32);
}

#[test]
fn side_tables_roundtrip_bincode() {
    let mut b = TraceBuffer::new();
    let x = b.fresh_ssa();
    b.record_type(x, ObservedType::Bool);
    b.record_const(x, TraceConst::Bool(true));
    b.append(TraceOp::ConstI32(x, 1));
    b.record_guard(GuardSite::new(
        0,
        relon_trace_jit::ExternalPc(42),
        GuardKind::NotNull(x),
    ));
    let opt = b.into_optimized();
    let tables = opt.side_tables();
    let bytes = bincode::serialize(&tables).expect("encode");
    let back: SerializableSideTables = bincode::deserialize(&bytes).expect("decode");
    assert_eq!(back, tables);
}

#[test]
fn call_op_records_inputs() {
    let mut b = TraceBuffer::new();
    let arg = b.fresh_ssa();
    let ret = b.fresh_ssa();
    b.append(TraceOp::ConstI64(arg, 9));
    b.append(TraceOp::Call(ret, FuncId(3), vec![arg], EffectClass::Pure));
    assert_eq!(b.op_count(), 2);
    assert_eq!(b.ops[1].inputs(), vec![arg]);
}

#[test]
fn default_pipeline_runs_three_passes_clean_buffer() {
    let mut b = TraceBuffer::new();
    let x = b.fresh_ssa();
    b.append(TraceOp::ConstI64(x, 10));
    let p = OptimizerPipeline::default_pipeline();
    let reports = p.run(&mut b);
    assert_eq!(reports.len(), 3);
    // Nothing to fold / spec / DSE in this trivial trace.
    for (_, r) in reports {
        assert!(!r.touched());
    }
}

#[test]
fn store_op_has_two_inputs() {
    let op = TraceOp::Store(SsaVar(0), Offset(8), SsaVar(1));
    assert_eq!(op.inputs(), vec![SsaVar(0), SsaVar(1)]);
    assert!(op.output().is_none());
}
