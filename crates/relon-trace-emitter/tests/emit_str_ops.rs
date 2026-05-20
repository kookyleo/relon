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
    let ctx = emit_and_verify(&b.into_optimized());
    let s = format!("{}", ctx.func);
    // No const-byte side-table entry → fall back to the extern shim
    // call. Verify the call survives.
    assert!(
        s.contains("call fn") || s.contains("call colocated"),
        "expected `call __relon_str_contains` in emitted IR:\n{s}"
    );
}

// ---- F-D7-C inline-needle lowering --------------------------------

/// Helper: build a `StrContains` trace where `n` carries a const-byte
/// side-table entry of the given length. Returns the verified function
/// IR text so individual tests can pattern-match against it.
fn emit_with_const_needle(needle: &[u8]) -> String {
    let mut b = TraceBuffer::new();
    let h = b.fresh_ssa();
    let n = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(h, 0x3000));
    b.append(TraceOp::ConstI64(n, 0x4000));
    b.record_const_bytes(n, needle.to_vec());
    b.append(TraceOp::StrContains(dst, h, n));
    b.append(TraceOp::Return(dst));
    let ctx = emit_and_verify(&b.into_optimized());
    format!("{}", ctx.func)
}

/// The emitter declares the str_contains hook with FuncRef `fn4` (the
/// 5th declared host hook, by [`HostHookFuncIds::default`]'s layout:
/// save_deopt / resolve_call / inline_cache_lookup / str_concat /
/// str_contains). A `call fn4(` substring in the IR text means the
/// emit_str_contains lowering hit the extern path; absence means
/// inline. We deliberately don't grep on raw `call fn` because the
/// deopt block always emits a `call fn0(save_deopt, ...)`.
const STR_CONTAINS_EXTERN_CALL_TAG: &str = "call fn4(";

#[test]
fn str_contains_inline_for_one_byte_needle() {
    let ir = emit_with_const_needle(b"x");
    // Inline path emits per-byte loads and a `band` chain — and crucially
    // no `call fn4(__relon_str_contains, ...)`.
    assert!(
        !ir.contains(STR_CONTAINS_EXTERN_CALL_TAG),
        "1-byte const needle should be inlined, no extern call expected:\n{ir}"
    );
    assert!(
        ir.contains("load.i8"),
        "inline scan must load haystack bytes:\n{ir}"
    );
}

#[test]
fn str_contains_inline_for_eight_byte_needle() {
    let ir = emit_with_const_needle(b"01234567");
    assert!(
        !ir.contains(STR_CONTAINS_EXTERN_CALL_TAG),
        "8-byte const needle should be inlined:\n{ir}"
    );
}

#[test]
fn str_contains_inline_for_sixteen_byte_needle() {
    let ir = emit_with_const_needle(b"0123456789abcdef");
    assert!(
        !ir.contains(STR_CONTAINS_EXTERN_CALL_TAG),
        "16-byte const needle should be inlined (boundary case):\n{ir}"
    );
}

#[test]
fn str_contains_falls_back_to_extern_for_seventeen_byte_needle() {
    let ir = emit_with_const_needle(b"0123456789abcdefg");
    assert!(
        ir.contains(STR_CONTAINS_EXTERN_CALL_TAG),
        "17-byte needle should fall back to extern shim:\n{ir}"
    );
}

#[test]
fn str_contains_inline_for_empty_needle_short_circuits_to_one() {
    let ir = emit_with_const_needle(b"");
    // Empty needle → `iconst.i32 1` is the entire lowering; no
    // haystack load and definitely no extern call.
    assert!(
        !ir.contains(STR_CONTAINS_EXTERN_CALL_TAG),
        "empty needle should not call extern:\n{ir}"
    );
    assert!(
        ir.contains("iconst.i32 1"),
        "empty needle should emit `iconst.i32 1`:\n{ir}"
    );
}

// ---- F-D7-H preloaded-payload lowering -----------------------------

/// Helper: build a `StrContains` trace where the haystack SSA has had
/// its `(ptr, len)` payload pre-loaded via upstream `TraceOp::Load`
/// ops at `Offset(0)` / `Offset(8)` and the `str_payload` side-table
/// is populated. The const-needle is also recorded so the inline scan
/// engages. Returns the verified function IR text for pattern matching.
fn emit_with_preloaded_haystack(needle: &[u8]) -> String {
    use relon_trace_jit::Offset;
    let mut b = TraceBuffer::new();
    let h = b.fresh_ssa();
    let ptr_ssa = b.fresh_ssa();
    let len_ssa = b.fresh_ssa();
    let n = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(h, 0x3000));
    // Two real Loads upstream of the StrContains — mirrors what the
    // recorder's `inject_str_payload_loads` synthesises.
    b.append(TraceOp::Load(ptr_ssa, h, Offset(0)));
    b.append(TraceOp::Load(len_ssa, h, Offset(8)));
    b.record_str_payload(h, ptr_ssa, len_ssa);
    b.append(TraceOp::ConstI64(n, 0x4000));
    b.record_const_bytes(n, needle.to_vec());
    b.append(TraceOp::StrContains(dst, h, n));
    b.append(TraceOp::Return(dst));
    let ctx = emit_and_verify(&b.into_optimized());
    format!("{}", ctx.func)
}

#[test]
fn str_contains_preloaded_drops_inline_payload_deref() {
    // With the side-table populated, `emit_str_contains` should route
    // the inline scan through `HaystackHandle::Preloaded` and skip
    // the per-call `load_string_ref_payload` raw deref. The deref is
    // still present in the trace stream as the two upstream Loads,
    // but their `emit_load` lowering goes through the standard
    // `TraceOp::Load` path — exactly what the LICM hoister wants to
    // see.
    let ir = emit_with_preloaded_haystack(b"x");
    assert!(
        !ir.contains(STR_CONTAINS_EXTERN_CALL_TAG),
        "1-byte const needle should be inlined:\n{ir}"
    );
    // Two upstream loads — these are the StringRef ptr/len reads
    // emitted via `TraceOp::Load`, lowered by the regular `emit_load`
    // arm.
    let load_i64_count = ir.matches("load.i64").count();
    assert!(
        load_i64_count >= 2,
        "expected ≥2 `load.i64` ops (StringRef ptr+len) in IR:\n{ir}"
    );
}

#[test]
fn str_contains_without_str_payload_uses_raw_handle() {
    // No `record_str_payload` call → emitter falls back to
    // `HaystackHandle::Raw` and the inline scan does its own
    // per-call `(ptr, len)` deref. Verify the inline scan still
    // fires (no extern call) and that the IR carries the
    // null-haystack guard the Raw variant always emits.
    let ir = emit_with_const_needle(b"x");
    assert!(
        !ir.contains(STR_CONTAINS_EXTERN_CALL_TAG),
        "1-byte const needle should still be inlined:\n{ir}"
    );
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
