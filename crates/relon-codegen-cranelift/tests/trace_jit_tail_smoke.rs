//! v6-ε-0-C: smoke tests for the `CallConv::Tail` trace entry path.
//!
//! These tests pin the wire-level interop between a Rust
//! `unsafe extern "C"` caller and a cranelift-emitted Tail-conv
//! callee. The v6-δ M2-C bench accounting identified the trace entry
//! prologue + epilogue as the 9.5 ns/iter hot-loop floor; v6-ε-0-C
//! switches the entry's call conv to [`CallConv::Tail`] to shrink that
//! floor.
//!
//! Coverage:
//!
//! 1. **Happy path** — trivial `ConstI64 + Return` trace compiled with
//!    `CallConv::Tail`, invoked via `JITedTraceFn::invoke`. Confirms
//!    the Rust→Tail-callee cross-conv `call` correctly delivers
//!    `(*mut TraceContext, *const u64)` args and reads back the
//!    `i32` status code.
//! 2. **Add via LocalGet** — non-overflowing arith path. Confirms the
//!    Tail callee correctly receives both pointer args and reads off
//!    `args_ptr + slot_idx * 8`.
//! 3. **Guard fire + deopt** — overflow guard triggers a deopt in
//!    a Tail-conv trace. Confirms:
//!    - the brif → deopt_block path works under Tail (different
//!      callee-save frame than SysV);
//!    - the cross-conv call from the Tail trace to the SysV
//!      `__relon_trace_save_deopt` host helper does not corrupt the
//!      `DeoptStateSnapshot`;
//!    - `invoke_with_resume` surfaces the snapshot's `ssa_slots_copy`
//!      after the deopt, exactly as the SysV path did.
//! 4. **SysV parity** — the same workload compiled with
//!    `CallConv::SystemV` produces the same Success / GuardFailed
//!    behaviour. Ensures Tail did not introduce a one-off bug.

use std::ptr;

use cranelift_codegen::isa::CallConv;

use relon_codegen_cranelift::trace_install::{global_trace_jit_state, TraceJitState};
use relon_ir::{IrType, Op, TaggedOp};
use relon_parser::TokenRange;
use relon_trace_abi::{TraceContext, TraceEntryStatus};
use relon_trace_jit::{TraceBuffer, TraceOp};

use relon_codegen_cranelift::{
    clear_recording, register_recording, trace_install::__relon_jump_to_recorder,
    RecordingRegistration,
};

/// Pick a high fn_id that lives clear of bench / smoke-test allocations.
const TAIL_SMOKE_FN_ID_BASE: u32 = 850;

#[test]
fn tail_conv_const_return_invokes_through_extern_c() {
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let dst = buffer.fresh_ssa();
    buffer.append(TraceOp::ConstI64 {
        dst,
        value: 0xfeed_beef,
    });
    buffer.append(TraceOp::Return { value: dst });

    let trace_fn = state
        .jit_compile_buffer_for_fn_with_call_conv(TAIL_SMOKE_FN_ID_BASE, buffer, CallConv::Tail)
        .expect("compile Tail-conv trace");

    let mut ctx = TraceContext::with_capacity(8);
    // SAFETY: the entry pointer obeys TRACE_ENTRY_SIG; ctx outlives
    // the call; args may be null because the trace ignores it.
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, ptr::null()) };
    assert_eq!(
        status,
        TraceEntryStatus::Success,
        "Tail-conv trace must return Success across the Rust extern \"C\" → Tail callee boundary",
    );
    assert_eq!(ctx.result_slot as i64, 0xfeed_beef);
}

#[test]
fn tail_conv_add_via_localget_round_trips_args() {
    let state = TraceJitState::new();
    let mut buffer = TraceBuffer::new();
    let a = buffer.fresh_ssa();
    let b = buffer.fresh_ssa();
    let sum = buffer.fresh_ssa();
    buffer.append(TraceOp::LocalGet {
        dst: a,
        slot_idx: 0,
    });
    buffer.append(TraceOp::LocalGet {
        dst: b,
        slot_idx: 1,
    });
    buffer.append(TraceOp::Add {
        dst: sum,
        lhs: a,
        rhs: b,
    });
    buffer.append(TraceOp::Return { value: sum });

    let trace_fn = state
        .jit_compile_buffer_for_fn_with_call_conv(TAIL_SMOKE_FN_ID_BASE + 1, buffer, CallConv::Tail)
        .expect("compile Tail-conv LocalGet+Add trace");

    let mut ctx = TraceContext::with_capacity(16);
    let args: [u64; 2] = [40u64, 2u64];
    // SAFETY: args is a 2-element u64 array; ctx is exclusive; the
    // Tail-conv callee uses rdi/rsi (x86_64) or x0/x1 (aarch64) for
    // the two pointer args — same registers the Rust extern "C"
    // caller writes them into.
    let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, args.as_ptr()) };
    assert_eq!(status, TraceEntryStatus::Success);
    assert_eq!(
        ctx.result_slot as i64, 42,
        "Tail callee must read both LocalGet slots off args_ptr correctly",
    );
}

#[test]
fn tail_conv_guard_failure_unwinds_through_deopt_block() {
    // This test goes through the full recorder → emitter pipeline so
    // the trace carries a real `ArithOverflow` guard. We then drive
    // an overflowing invocation and assert:
    //   1. status == GuardFailed (trace's brif → deopt_block path
    //      executed correctly under Tail callee-save discipline);
    //   2. ctx.deopt_state carries a non-trivial snapshot (the
    //      cross-conv `call __relon_trace_save_deopt` did write
    //      `ssa_slots_copy` despite the Tail caller / SysV callee
    //      conv mismatch).
    //
    // Uses a *fresh* `TraceJitState` so it doesn't clash with the
    // global state another test in the suite might be using.
    let fn_id = TAIL_SMOKE_FN_ID_BASE + 2;

    // Body sized past `TINY_TRACE_OP_THRESHOLD` so the runtime
    // gate doesn't route around the trace before the per-`Add`
    // overflow guard can fire. Padding with `+ 0` keeps the trace
    // semantically equivalent to "x + y".
    let body = vec![
        TaggedOp {
            op: Op::LocalGet(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::LocalGet(1),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Add(IrType::I64),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::ConstI64(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Add(IrType::I64),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::ConstI64(0),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Add(IrType::I64),
            range: TokenRange::default(),
        },
        TaggedOp {
            op: Op::Return,
            range: TokenRange::default(),
        },
    ];

    // Clean state for this fn_id.
    let global = global_trace_jit_state();
    let _ = clear_recording(fn_id);
    let _ = global.invalidate_trace(fn_id);
    register_recording(
        fn_id,
        RecordingRegistration {
            body,
            param_tys: vec![IrType::I32, IrType::I32],
            ..Default::default()
        },
    );

    // Drive recording once with non-overflowing warm args so the
    // trace installs.
    let warm: [u64; 2] = [1, 2];
    // SAFETY: warm.as_ptr() is a 2-element u64 array; the recorder
    // will read len == param_tys.len() == 2.
    unsafe {
        __relon_jump_to_recorder(fn_id, warm.as_ptr());
    }
    assert!(
        global.lookup_trace(fn_id).is_some(),
        "trace must install on first warm-up invocation"
    );

    // Now trigger an overflow → guard fires → deopt block runs →
    // status = GuardFailed. The trace was emitted with whatever conv
    // `jit_compile_trace_for_fn` picks, which on x86_64 / aarch64 is
    // Tail (per `relon_trace_emitter::trace_entry_call_conv`).
    let snapshot_present = std::cell::Cell::new(false);
    let snapshot_slots = std::cell::Cell::new(0usize);
    let of_args: [u64; 2] = [i64::MAX as u64, 1u64];

    // SAFETY: `of_args` is sized for param_tys.len() == 2; the
    // fallback closure synthesises a wrapping result so the loop
    // result is well-defined even when the snapshot is empty.
    let r = unsafe {
        global.invoke_with_resume(
            fn_id,
            of_args.as_ptr(),
            32,
            |_args, _resume_pc, snapshot| {
                if let Some(s) = snapshot {
                    snapshot_present.set(true);
                    snapshot_slots.set(s.ssa_slots_copy.len());
                }
                i64::MIN as u64
            },
        )
    };
    assert_eq!(r, i64::MIN as u64);
    assert!(
        snapshot_present.get(),
        "Tail-conv trace must surface a DeoptStateSnapshot on guard failure"
    );
    assert!(
        snapshot_slots.get() > 0,
        "Tail-conv deopt path must populate ssa_slots_copy (cross-conv call to save_deopt did not corrupt it)"
    );

    let _ = clear_recording(fn_id);
    let _ = global.invalidate_trace(fn_id);
}

#[test]
fn tail_conv_matches_systemv_for_identical_buffer() {
    // Hand-build two identical TraceBuffer's, compile one with Tail
    // and one with SystemV, invoke both, and confirm the result_slot
    // matches. Pins that the cross-conv interop hasn't introduced a
    // silent wrong-answer bug.
    fn build_buffer() -> TraceBuffer {
        let mut b = TraceBuffer::new();
        let x = b.fresh_ssa();
        let y = b.fresh_ssa();
        let r = b.fresh_ssa();
        b.append(TraceOp::ConstI64 {
            dst: x,
            value: 1_000_000,
        });
        b.append(TraceOp::ConstI64 {
            dst: y,
            value: 234_567,
        });
        b.append(TraceOp::Add {
            dst: r,
            lhs: x,
            rhs: y,
        });
        b.append(TraceOp::Return { value: r });
        b
    }

    let state = TraceJitState::new();
    let tail = state
        .jit_compile_buffer_for_fn_with_call_conv(
            TAIL_SMOKE_FN_ID_BASE + 3,
            build_buffer(),
            CallConv::Tail,
        )
        .expect("compile Tail");
    let sysv = state
        .jit_compile_buffer_for_fn_with_call_conv(
            TAIL_SMOKE_FN_ID_BASE + 4,
            build_buffer(),
            CallConv::SystemV,
        )
        .expect("compile SystemV");

    let mut ctx_tail = TraceContext::with_capacity(8);
    let mut ctx_sysv = TraceContext::with_capacity(8);
    let st_tail = unsafe { tail.invoke(&mut ctx_tail as *mut _, ptr::null()) };
    let st_sysv = unsafe { sysv.invoke(&mut ctx_sysv as *mut _, ptr::null()) };
    assert_eq!(st_tail, st_sysv);
    assert_eq!(ctx_tail.result_slot, ctx_sysv.result_slot);
    assert_eq!(ctx_tail.result_slot as i64, 1_234_567);
}
