//! v6-γ M4: every `TraceOp::Guard` the recorder appends to the
//! buffer must also have a matching [`GuardSite`] in the buffer's
//! side-table.
//!
//! Before the M4 fix the recorder appended the linear op without
//! calling [`TraceBuffer::record_guard`]; the emitter then surfaced
//! `EmitError::OrphanGuardOp` on every arith / div / load trace. The
//! tests here pin the post-fix invariant: for every `Guard` op in
//! `buf.ops` there is exactly one `GuardSite` in `buf.guards` whose
//! `trace_pc` matches the op's position.

use relon_ir::{IrType, Op};
use relon_trace_jit::{GuardKind, ObservedType, TraceOp};
use relon_trace_recorder::{RecordResult, RecorderState};

fn const_i64(r: &mut RecorderState, v: i64) -> relon_trace_jit::SsaVar {
    match r.record_op(&Op::ConstI64(v), &[], Some(ObservedType::I64)) {
        RecordResult::Ok { value: Some(s) } => s,
        other => panic!("ConstI64({v}) -> {other:?}"),
    }
}

/// Every guard op in the linear stream must have a matching site in
/// the buffer's `guards` table.
fn assert_guards_balanced(buf: &relon_trace_jit::TraceBuffer) {
    let mut guard_pcs: Vec<u32> = buf
        .ops
        .iter()
        .enumerate()
        .filter(|(_, o)| matches!(o, TraceOp::Guard(_, _)))
        .map(|(pc, _)| pc as u32)
        .collect();
    guard_pcs.sort_unstable();
    let mut site_pcs: Vec<u32> = buf.guards.iter().map(|g| g.trace_pc).collect();
    site_pcs.sort_unstable();
    assert_eq!(
        guard_pcs, site_pcs,
        "every TraceOp::Guard must have a matching GuardSite (post v6-γ M4)"
    );
}

#[test]
fn add_overflow_guard_has_matching_site() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 11);
    let b = const_i64(&mut r, 22);
    let _ = r.record_op(&Op::Add(IrType::I64), &[b, a], Some(ObservedType::I64));
    let buf = r.finalize().expect("no abort");
    assert_guards_balanced(&buf);
    assert_eq!(
        buf.guards.len(),
        1,
        "Add(I64) emits exactly one ArithOverflow guard"
    );
    assert!(matches!(
        buf.guards[0].kind,
        GuardKind::ArithOverflow(_)
    ));
}

#[test]
fn sub_overflow_guard_has_matching_site() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 100);
    let b = const_i64(&mut r, 1);
    let _ = r.record_op(&Op::Sub(IrType::I64), &[b, a], Some(ObservedType::I64));
    let buf = r.finalize().expect("no abort");
    assert_guards_balanced(&buf);
    assert_eq!(buf.guards.len(), 1);
}

#[test]
fn mul_overflow_guard_has_matching_site() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 6);
    let b = const_i64(&mut r, 7);
    let _ = r.record_op(&Op::Mul(IrType::I64), &[b, a], Some(ObservedType::I64));
    let buf = r.finalize().expect("no abort");
    assert_guards_balanced(&buf);
    assert_eq!(buf.guards.len(), 1);
}

#[test]
fn div_overflow_guard_has_matching_site() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 12);
    let b = const_i64(&mut r, 4);
    let _ = r.record_op(&Op::Div(IrType::I64), &[b, a], Some(ObservedType::I64));
    let buf = r.finalize().expect("no abort");
    assert_guards_balanced(&buf);
    assert_eq!(buf.guards.len(), 1);
}

#[test]
fn multiple_arith_ops_all_have_matching_sites() {
    let mut r = RecorderState::new();
    let a = const_i64(&mut r, 1);
    let b = const_i64(&mut r, 2);
    let _ = r.record_op(&Op::Add(IrType::I64), &[b, a], Some(ObservedType::I64));
    let c = const_i64(&mut r, 3);
    let _ = r.record_op(&Op::Mul(IrType::I64), &[c, a], Some(ObservedType::I64));
    let buf = r.finalize().expect("no abort");
    assert_guards_balanced(&buf);
    // Each of Add + Mul produces one ArithOverflow guard site.
    assert_eq!(buf.guards.len(), 2);
}

#[test]
fn no_arith_no_guards() {
    let mut r = RecorderState::new();
    let val = const_i64(&mut r, 42);
    let _ = r.record_op(&Op::Return, &[val], None);
    let buf = r.finalize().expect("no abort");
    assert_guards_balanced(&buf);
    assert!(buf.guards.is_empty(), "const + return → zero guards");
}

#[test]
fn external_pc_overrideable() {
    let mut r = RecorderState::new();
    r.set_next_external_pc(0xdead_beef);
    let a = const_i64(&mut r, 1);
    let b = const_i64(&mut r, 2);
    let _ = r.record_op(&Op::Add(IrType::I64), &[b, a], Some(ObservedType::I64));
    let buf = r.finalize().expect("no abort");
    assert_eq!(buf.guards.len(), 1);
    assert_eq!(buf.guards[0].deopt_pc.0, 0xdead_beef);
}
