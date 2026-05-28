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
//! Why this lives in `relon-codegen-cranelift`: the trace install path
//! owns the host-symbol registration for `__relon_str_concat_n_alloc`
//! and the JIT module finalisation; the trace-emitter crate would
//! need an entire test harness to reach the same point.

use relon_codegen_cranelift::trace_install::{MaterialisedValue, ReturnKind, TraceJitState};
use relon_trace_abi::TraceContext;
use relon_trace_jit::runtime::{__relon_str_concat, StringRef};
use relon_trace_jit::{TraceBuffer, TraceOp};

/// Build a `TraceOp::StrConcatN` trace whose `operand_count = N`
/// operands come from the entry's packed `args` slots (`LocalGet(0)
/// .. LocalGet(N-1)`), then `Return` the resulting `*const StringRef`.
fn build_concat_n_trace(n: u32) -> TraceBuffer {
    let mut b = TraceBuffer::new();
    let mut operands = Vec::with_capacity(n as usize);
    for slot in 0..n {
        let v = b.fresh_ssa();
        b.append(TraceOp::LocalGet {
            dst: v,
            slot_idx: slot,
        });
        operands.push(v);
    }
    let dst = b.fresh_ssa();
    b.append(TraceOp::StrConcatN { dst, operands });
    b.append(TraceOp::Return { value: dst });
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

    // Capture the oracle payload **before** invoking the trace —
    // `invoke_materialised` reclaims the trace string arena after
    // copying the payload, which would dangle the oracle pointer.
    let oracle_ptr = oracle_concat(&operand_ptrs);
    let oracle_payload = read_payload(oracle_ptr);

    let hooks = relon_codegen_cranelift::default_host_hooks();
    let mut ctx = TraceContext::with_hooks(64, hooks);
    // Review #178 P2: high-level invoke returns an owned SmolStr;
    // caller never touches the arena `*const StringRef`.
    let val = unsafe {
        jited
            .invoke_materialised(&mut ctx as *mut _, args.as_ptr(), ReturnKind::String)
            .expect("StrConcatN install + invoke must succeed")
    };
    let trace_payload: Vec<u8> = match val {
        MaterialisedValue::String(s) => s.as_str().as_bytes().to_vec(),
        other => panic!("expected MaterialisedValue::String, got {other:?}"),
    };

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

    // Keep the JIT module alive past the SmolStr move — the bytes
    // are owned now but the trace fn pointer ride the `jited` Arc.
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
    // Drive the same installed trace through a hot loop so the install
    // / re-invoke path exercises repeated allocation through
    // `__relon_str_concat_n_alloc`. Review #178 P2:
    // `invoke_materialised` reclaims the trace string arena after
    // each invoke — including the operand `StringRef`s registered by
    // `from_static`. Production hosts handle this by interning the
    // operand strings outside the trace arena; the test mirrors the
    // shape by re-registering the operands inside the loop so each
    // iter starts with a fresh arena.
    let trace = build_concat_n_trace(3);
    let state = TraceJitState::new();
    let jited = state.jit_compile_buffer_for_fn(42, trace).expect("install");
    let hooks = relon_codegen_cranelift::default_host_hooks();
    for iter in 0..32 {
        let operands = [
            StringRef::from_static("L_"),
            StringRef::from_static("M_"),
            StringRef::from_static("R"),
        ];
        let args: [u64; 3] = [operands[0] as u64, operands[1] as u64, operands[2] as u64];
        let mut ctx = TraceContext::with_hooks(64, hooks);
        let val = unsafe {
            jited
                .invoke_materialised(&mut ctx as *mut _, args.as_ptr(), ReturnKind::String)
                .unwrap_or_else(|e| panic!("iter {iter}: invoke must Succeed; got {e:?}"))
        };
        match val {
            MaterialisedValue::String(s) => {
                assert_eq!(
                    s.as_str().as_bytes(),
                    b"L_M_R",
                    "iter {iter}: payload drift"
                );
            }
            other => panic!("iter {iter}: expected String, got {other:?}"),
        }
        // The reclaim that runs inside invoke_materialised drains the
        // per-iter operand allocations along with the trace's result —
        // confirms the arena is fully drained between iters, which is
        // the property the original test (`invoke_raw` + manual
        // pointer read) could not assert without re-implementing the
        // reclaim path here.
        assert_eq!(
            relon_trace_jit::runtime::trace_string_arena_len(),
            0,
            "iter {iter}: arena must be drained after invoke_materialised"
        );
    }
}
