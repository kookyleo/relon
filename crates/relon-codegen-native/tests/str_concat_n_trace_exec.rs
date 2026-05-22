//! #168 end-to-end: install a `TraceOp::StrConcatN` trace via the
//! full trace-JIT install pipeline (`TraceJitState`) and confirm the
//! result matches a left-to-right chain of `__relon_str_concat`
//! invocations.
//!
//! The trace is hand-built rather than driven through the full
//! recorder pipeline so the assertion focuses on the StrConcatN
//! emitter / runtime helper round-trip. Recorder-side lowering has
//! its own unit-test coverage under
//! `crates/relon-trace-recorder/src/lowering.rs::tests`.
//!
//! Why this lives in `relon-codegen-native`: the trace install path
//! owns the host-symbol registration for `__relon_str_concat_n_alloc`
//! and the JIT module finalisation; the trace-emitter crate would
//! need an entire test harness to reach the same point.

use relon_codegen_native::trace_install::TraceJitState;
use relon_trace_abi::{ObservedType, TraceContext};
use relon_trace_jit::runtime::{
    __relon_str_concat, reclaim_trace_strings, trace_string_arena_len, StringRef,
};
use relon_trace_jit::{TraceBuffer, TraceOp};

/// Build a `TraceOp::StrConcatN` trace whose `operand_count = N`
/// operands come from the entry's packed `args` slots (`LocalGet(0)
/// .. LocalGet(N-1)`), then `Return` the resulting `*const StringRef`.
fn build_concat_n_trace(n: u32) -> TraceBuffer {
    let mut b = TraceBuffer::new();
    let mut operands = Vec::with_capacity(n as usize);
    for slot in 0..n {
        let v = b.fresh_ssa();
        b.append(TraceOp::LocalGet(v, slot));
        operands.push(v);
    }
    let dst = b.fresh_ssa();
    b.append(TraceOp::StrConcatN { dst, operands });
    b.append(TraceOp::Return(dst));
    b
}

/// Walk the operands left-to-right and chain `__relon_str_concat`
/// invocations the way the recorder's `step_str_concat_n` does at
/// recording time. Used as the oracle for the installed trace.
fn oracle_concat(operands: &[*const StringRef]) -> *const StringRef {
    assert!(!operands.is_empty());
    let mut acc = operands[0];
    for p in &operands[1..] {
        acc = unsafe { __relon_str_concat(acc, *p) };
    }
    acc
}

fn read_payload(p: *const StringRef) -> Vec<u8> {
    if p.is_null() {
        return Vec::new();
    }
    let r = unsafe { &*p };
    if r.ptr.is_null() {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(r.ptr, r.len).to_vec() }
}

/// Drive a `StrConcatN` trace with N operands through the full
/// install + invoke pipeline and confirm the result_slot pointer
/// payload matches the left-to-right shim chain.
fn assert_concat_n_matches_shim(operands_text: &[&'static str]) {
    unsafe { reclaim_trace_strings() };
    assert_eq!(trace_string_arena_len(), 0);

    let n = operands_text.len() as u32;
    let trace = build_concat_n_trace(n);

    let state = TraceJitState::new();
    let fn_id = n;
    let jited = state
        .jit_compile_buffer_for_fn(fn_id, trace)
        .expect("StrConcatN trace install must succeed when host hooks are wired");

    // Materialise host-side StringRef inputs and pack their raw
    // pointers as u64s into the entry-fn args array. The trace's
    // `LocalGet(slot)` lowering loads `args_ptr[slot]` as i64.
    let operand_ptrs: Vec<*const StringRef> = operands_text
        .iter()
        .map(|s| StringRef::from_static(s))
        .collect();
    let args: Vec<u64> = operand_ptrs.iter().map(|p| *p as u64).collect();

    let hooks = relon_codegen_native::default_host_hooks();
    let mut ctx = TraceContext::with_hooks(64, hooks);
    let (status, trace_payload, oracle_payload) = unsafe {
        jited.invoke_with_string_reclaim(&mut ctx as *mut _, args.as_ptr(), |status, ctx| {
            let trace_result_ptr = ctx.result_slot as *const StringRef;
            let trace_payload = read_payload(trace_result_ptr);

            let oracle_ptr = oracle_concat(&operand_ptrs);
            let oracle_payload = read_payload(oracle_ptr);

            (status, trace_payload, oracle_payload)
        })
    };
    assert_eq!(
        status,
        relon_trace_abi::TraceEntryStatus::Success,
        "StrConcatN install + invoke must succeed; got {status:?}"
    );
    assert_eq!(
        trace_string_arena_len(),
        0,
        "scoped invoke must reclaim trace/input/oracle StringRefs"
    );

    assert_eq!(
        trace_payload, oracle_payload,
        "StrConcatN trace payload must match the left-to-right shim \
         chain; trace={trace_payload:?} oracle={oracle_payload:?}"
    );

    // Sanity: the helper concatenates left-to-right so the payload
    // bytes should equal the source-order join with no separator.
    let expected: Vec<u8> = operands_text
        .iter()
        .flat_map(|s| s.as_bytes().to_vec())
        .collect();
    assert_eq!(
        trace_payload, expected,
        "concat-n payload must equal the source-order byte join"
    );

    // Keep the JIT module alive for the duration of the result read;
    // `invoke_with_string_reclaim` materialised the payload before
    // reclaiming the trace string arena.
    let _ = &jited;
}

#[test]
fn str_concat_n_three_operands_matches_shim_chain() {
    assert_concat_n_matches_shim(&["foo", "-bar", "-baz"]);
}

#[test]
fn str_concat_n_four_operands_matches_shim_chain() {
    assert_concat_n_matches_shim(&["a", "bb", "ccc", "dddd"]);
}

#[test]
fn str_concat_n_three_operands_with_empty_segments() {
    assert_concat_n_matches_shim(&["", "x", ""]);
}

#[test]
fn str_concat_n_three_operands_drives_a_hot_loop() {
    unsafe { reclaim_trace_strings() };
    assert_eq!(trace_string_arena_len(), 0);

    // Drive the same installed trace through a hot loop so the install
    // / re-invoke path exercises repeated allocation through
    // `__relon_str_concat_n_alloc`; the scoped invoke must reclaim the
    // per-iter StringRefs every time.
    let trace = build_concat_n_trace(3);
    let state = TraceJitState::new();
    let jited = state.jit_compile_buffer_for_fn(42, trace).expect("install");
    let hooks = relon_codegen_native::default_host_hooks();
    for iter in 0..32 {
        let operands = [
            StringRef::from_static("L_"),
            StringRef::from_static("M_"),
            StringRef::from_static("R"),
        ];
        let args: [u64; 3] = [operands[0] as u64, operands[1] as u64, operands[2] as u64];
        let mut ctx = TraceContext::with_hooks(64, hooks);
        let (status, payload) = unsafe {
            jited.invoke_with_string_reclaim(&mut ctx as *mut _, args.as_ptr(), |status, ctx| {
                (status, read_payload(ctx.result_slot as *const StringRef))
            })
        };
        assert_eq!(
            status,
            relon_trace_abi::TraceEntryStatus::Success,
            "iter {iter}: hot-loop invoke of StrConcatN trace must Succeed"
        );
        assert_eq!(payload, b"L_M_R", "iter {iter}: payload drift");
        assert_eq!(
            trace_string_arena_len(),
            0,
            "iter {iter}: scoped invoke must reclaim StringRefs"
        );
    }
}

#[test]
fn invoke_with_resume_reclaims_string_temps_for_numeric_success() {
    unsafe { reclaim_trace_strings() };
    assert_eq!(trace_string_arena_len(), 0);

    let mut trace = TraceBuffer::new();
    let mut operands_ssa = Vec::new();
    for slot in 0..3 {
        let v = trace.fresh_ssa();
        trace.append(TraceOp::LocalGet(v, slot));
        operands_ssa.push(v);
    }
    let tmp = trace.fresh_ssa();
    trace.append(TraceOp::StrConcatN {
        dst: tmp,
        operands: operands_ssa,
    });
    let ret = trace.fresh_ssa();
    trace.append(TraceOp::ConstI64(ret, 77));
    trace.record_type(ret, ObservedType::I64);
    trace.append(TraceOp::Return(ret));

    let state = TraceJitState::new();
    let fn_id = 77;
    let jited = state
        .jit_compile_buffer_for_fn(fn_id, trace)
        .expect("install numeric-return trace");
    state.install_trace(fn_id, jited);

    let operands = [
        StringRef::from_static("tmp"),
        StringRef::from_static("-"),
        StringRef::from_static("str"),
    ];
    let args: [u64; 3] = [operands[0] as u64, operands[1] as u64, operands[2] as u64];

    let result = unsafe {
        state.invoke_with_resume(fn_id, args.as_ptr(), 64, |_args, _pc, _snapshot| {
            panic!("numeric success trace must not fall back")
        })
    };
    assert_eq!(result, 77);
    assert_eq!(
        trace_string_arena_len(),
        0,
        "numeric-success invoke_with_resume should reclaim string temporaries"
    );
}
