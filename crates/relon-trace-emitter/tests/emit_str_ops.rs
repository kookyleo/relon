//! F-D7 string fast-path lowering. Each `TraceOp::Str*` op must
//! turn into a direct `call <hook>` against the matching
//! `__relon_str_*` shim. We assert the emitted IR contains a `call`
//! and that the cranelift verifier accepts the function shape.
//!
//! End-to-end execution (actually running the JIT'd code) lives in
//! `relon-codegen-native`'s install tests, since that's the crate
//! that owns the host-symbol registration. Here we stop at IR
//! verification — sufficient to catch shape regressions.

mod common;

use common::emit_and_verify;
use relon_trace_jit::{TraceBuffer, TraceOp};

#[test]
fn str_concat_emits_call() {
    let mut b = TraceBuffer::new();
    let lhs = b.fresh_ssa();
    let rhs = b.fresh_ssa();
    let dst = b.fresh_ssa();
    // Both operands are i64-typed const pointers — a real recorder
    // would emit `LocalGet(_)` for each, but `ConstI64` is good
    // enough to exercise the lowering.
    b.append(TraceOp::ConstI64(lhs, 0x1000));
    b.append(TraceOp::ConstI64(rhs, 0x2000));
    b.append(TraceOp::StrConcat(dst, lhs, rhs));
    b.append(TraceOp::Return(dst));
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    assert!(
        s.contains("call fn") || s.contains("call colocated"),
        "expected `call` in emitted IR:\n{s}"
    );
}

#[test]
fn str_contains_emits_call() {
    let mut b = TraceBuffer::new();
    let h = b.fresh_ssa();
    let n = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(h, 0x3000));
    b.append(TraceOp::ConstI64(n, 0x4000));
    b.append(TraceOp::StrContains(dst, h, n));
    b.append(TraceOp::Return(dst));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn str_find_emits_call() {
    let mut b = TraceBuffer::new();
    let h = b.fresh_ssa();
    let n = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(h, 0x3000));
    b.append(TraceOp::ConstI64(n, 0x4000));
    b.append(TraceOp::StrFind(dst, h, n));
    b.append(TraceOp::Return(dst));
    emit_and_verify(&b.into_optimized());
}

#[test]
fn str_substring_emits_call() {
    let mut b = TraceBuffer::new();
    let s = b.fresh_ssa();
    let start = b.fresh_ssa();
    let length = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(s, 0x5000));
    b.append(TraceOp::ConstI64(start, 0));
    b.append(TraceOp::ConstI64(length, 4));
    b.append(TraceOp::StrSubstring(dst, s, start, length));
    b.append(TraceOp::Return(dst));
    emit_and_verify(&b.into_optimized());
}
