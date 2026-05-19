//! v6-ε-0-A: smoke tests for the at-call-site trace IR inline path.
//!
//! These tests exercise [`relon_codegen_native::compile_inline_host_fn`]
//! — the host-side companion to
//! [`relon_trace_emitter::emit_trace_inline`]. The contract:
//!
//! 1. The inline path produces a callable JIT entry obeying
//!    [`relon_trace_abi::TRACE_ENTRY_SIG`].
//! 2. For traces below [`relon_trace_emitter::MAX_INLINE_OPS`], the
//!    inline entry returns the same result the trampoline entry would
//!    have returned. (We compile the same `TraceBuffer` through both
//!    paths and bit-compare the result_slot.)
//! 3. For traces above the cap, [`compile_inline_host_fn`] surfaces
//!    [`InlineHostFnError::TraceTooLarge`] so the caller can fall
//!    back to the trampoline path.
//! 4. A guard fire inside an inlined trace dispatches through
//!    `ctx.host_hooks.save_deopt` and returns
//!    [`TraceEntryStatus::GuardFailed`]. The standalone trampoline's
//!    deopt semantics carry over: the bytecode-VM resume path can
//!    pick up `ssa_slots_copy` from the snapshot.
//!
//! ## Why "smoke" tests
//!
//! The bench (`trace_jit_warm_inline` row) covers the steady-state
//! cost; these tests cover the correctness invariants that the bench
//! is silent about. Together they discharge the v6-ε-0-A "Deopt path
//! preserved" deliverable.

use std::ptr;

use relon_codegen_native::{
    compile_inline_host_fn, trace_install::TraceJitState, InlineHostFnError,
};
use relon_trace_abi::{TraceContext, TraceEntryStatus};
use relon_trace_jit::{TraceBuffer, TraceOp};

/// Helper: build a const-return trace and inline-compile it.
fn inline_const_trace(value: i64) -> relon_codegen_native::InlineHostFn {
    let mut buffer = TraceBuffer::new();
    let dst = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64(dst, value));
    buffer.append(TraceOp::Return(dst));
    let trace = std::sync::Arc::new(buffer.into_optimized());
    compile_inline_host_fn(trace).expect("inline compile must succeed for trivial trace")
}

#[test]
fn inline_const_return_round_trips_through_extern_c() {
    let host_fn = inline_const_trace(0xfeed_beef);
    let mut ctx = TraceContext::with_capacity(8);
    // SAFETY: TRACE_ENTRY_SIG; args may be null since the trace has
    // no LocalGet ops.
    let raw = unsafe { (host_fn.typed_entry())(&mut ctx as *mut _, ptr::null()) };
    assert_eq!(raw, 0, "Success status code");
    assert_eq!(ctx.result_slot, 0xfeed_beef_u64);
}

#[test]
fn inline_add_localget_matches_trampoline() {
    // Build a `LocalGet(0) + LocalGet(1); Return` trace, compile it
    // through both the trampoline path (TraceJitState) and the inline
    // path (compile_inline_host_fn), then verify both return the same
    // result for the same input pair.
    let mut buffer_inline = TraceBuffer::new();
    let a_i = buffer_inline.fresh_ssa();
    let b_i = buffer_inline.fresh_ssa();
    let sum_i = buffer_inline.fresh_ssa();
    buffer_inline.append(TraceOp::LocalGet(a_i, 0));
    buffer_inline.append(TraceOp::LocalGet(b_i, 1));
    buffer_inline.append(TraceOp::Add(sum_i, a_i, b_i));
    buffer_inline.append(TraceOp::Return(sum_i));
    let trace_inline = std::sync::Arc::new(buffer_inline.into_optimized());
    let inline_fn = compile_inline_host_fn(trace_inline).expect("inline compile");

    // Trampoline path: same shape, different state instance.
    let mut buffer_tramp = TraceBuffer::new();
    let a_t = buffer_tramp.fresh_ssa();
    let b_t = buffer_tramp.fresh_ssa();
    let sum_t = buffer_tramp.fresh_ssa();
    buffer_tramp.append(TraceOp::LocalGet(a_t, 0));
    buffer_tramp.append(TraceOp::LocalGet(b_t, 1));
    buffer_tramp.append(TraceOp::Add(sum_t, a_t, b_t));
    buffer_tramp.append(TraceOp::Return(sum_t));
    let tramp_state = TraceJitState::new();
    let tramp_fn = tramp_state
        .jit_compile_buffer_for_fn(0, buffer_tramp)
        .expect("trampoline compile");

    // Drive both with identical inputs across a spread of non-overflow
    // ranges. Every iteration must round-trip the same i64 sum out of
    // both backends.
    let cases: [(i64, i64); 5] = [
        (1, 2),
        (-3, 7),
        (1_000_000, 2_000_000),
        (-100_000, 100_000),
        (0, 0),
    ];
    for (a, b) in cases {
        let args: [u64; 2] = [a as u64, b as u64];
        let mut ctx_i = TraceContext::with_capacity(16);
        let mut ctx_t = TraceContext::with_capacity(16);
        let raw_i = unsafe { (inline_fn.typed_entry())(&mut ctx_i as *mut _, args.as_ptr()) };
        let raw_t = unsafe { tramp_fn.invoke_raw(&mut ctx_t as *mut _, args.as_ptr()) };
        assert_eq!(raw_i, 0, "inline must Succeed for ({a}, {b})");
        assert_eq!(raw_t, 0, "trampoline must Succeed for ({a}, {b})");
        assert_eq!(
            ctx_i.result_slot, ctx_t.result_slot,
            "result_slot must match: ({a}, {b}) inline={} tramp={}",
            ctx_i.result_slot, ctx_t.result_slot
        );
        assert_eq!(ctx_i.result_slot as i64, a + b);
    }
}

#[test]
fn inline_guard_fire_routes_through_deopt_path() {
    // Build a trace that adds two operands with overflow-guarded
    // semantics. Feed it inputs that trigger the overflow guard so
    // we exercise the inline path's deopt block (which dispatches
    // through ctx.host_hooks.save_deopt the same way the trampoline
    // emitter does).
    use relon_trace_jit::{ExternalPc, GuardKind, GuardSite, SsaVar};

    let mut buffer = TraceBuffer::new();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let sum = buffer.fresh_ssa();
    buffer.append(TraceOp::LocalGet(a, 0));
    buffer.append(TraceOp::LocalGet(b, 1));
    let add_pc = buffer.append(TraceOp::Add(sum, a, b));
    let guard_pc = buffer.append(TraceOp::Guard(GuardKind::ArithOverflow(sum), SsaVar(0)));
    buffer.record_guard(
        GuardSite::new(
            guard_pc,
            ExternalPc(add_pc as u64),
            GuardKind::ArithOverflow(sum),
        )
        .with_ssa_stack_snapshot(vec![a, b, sum]),
    );
    buffer.append(TraceOp::Return(sum));

    let trace = std::sync::Arc::new(buffer.into_optimized());
    let inline_fn = compile_inline_host_fn(trace).expect("inline compile");

    // Inputs sized to trigger overflow on signed i64 add.
    let args: [u64; 2] = [i64::MAX as u64, 1u64];
    let mut ctx = TraceContext::with_hooks(16, relon_codegen_native::default_host_hooks());
    let raw = unsafe { (inline_fn.typed_entry())(&mut ctx as *mut _, args.as_ptr()) };
    assert_eq!(
        raw,
        TraceEntryStatus::GuardFailed.as_i32(),
        "overflow must surface GuardFailed"
    );
    // The deopt snapshot must have been written by the indirect
    // save_deopt dispatch arm.
    let snapshot = ctx.deopt_state.take();
    assert!(
        snapshot.is_some(),
        "deopt path must record snapshot via ctx.host_hooks.save_deopt"
    );
}

#[test]
fn inline_rejects_oversized_trace() {
    // Build a trace with MAX_INLINE_OPS + 2 ops so compile_inline_host_fn
    // surfaces TraceTooLarge without trying to emit.
    let mut buffer = TraceBuffer::new();
    let mut last = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64(last, 0));
    for _ in 0..(relon_trace_emitter::MAX_INLINE_OPS + 1) {
        last = buffer.fresh_ssa();
        buffer.append(TraceOp::ConstI64(last, 0));
    }
    buffer.append(TraceOp::Return(last));
    let trace = std::sync::Arc::new(buffer.into_optimized());
    match compile_inline_host_fn(trace) {
        Ok(_) => panic!("oversized trace must not compile inline"),
        Err(InlineHostFnError::TraceTooLarge { op_count, cap }) => {
            assert!(op_count > cap, "op_count {op_count} must exceed cap {cap}");
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn jited_trace_fn_exposes_inline_trace_for_re_emit() {
    // Pinning the v6-ε-0-A invariant: a trace compiled through the
    // standard install path retains its OptimizedTrace IR so a host
    // fn compiler can re-emit it inline. The standalone install
    // returns a JITedTraceFn carrying Arc<OptimizedTrace>; cloning
    // and feeding it into compile_inline_host_fn yields an inline
    // entry that returns the same value.
    let mut buffer = TraceBuffer::new();
    let dst = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64(dst, 0xc0ffee));
    buffer.append(TraceOp::Return(dst));
    let state = TraceJitState::new();
    let trace_fn = state
        .jit_compile_buffer_for_fn(42, buffer)
        .expect("trampoline compile");
    state.install_trace(42, trace_fn);

    let arc_trace = state
        .lookup_trace(42)
        .expect("must be installed")
        .inline_trace();
    // We can clone the retained IR and feed it through the inline
    // compiler — no extra recorder pass required.
    let inline_fn = compile_inline_host_fn(arc_trace).expect("inline re-compile");

    let mut ctx = TraceContext::with_capacity(8);
    let raw = unsafe { (inline_fn.typed_entry())(&mut ctx as *mut _, ptr::null()) };
    assert_eq!(raw, 0, "Success");
    assert_eq!(ctx.result_slot, 0xc0ffee);
}
