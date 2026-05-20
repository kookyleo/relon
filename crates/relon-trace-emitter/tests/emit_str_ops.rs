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

// ---- F-D7-I inline `StrConcat` short-rhs lowering ----------------

/// Emit a `StrConcat(lhs, rhs)` trace with the const-rhs side table
/// populated, going through `emit_with_hooks_and_call_conv` so we can
/// toggle the `str_concat_alloc` FuncId. Returns the verified IR text
/// alongside the assigned `str_concat_alloc` cranelift `fn?` tag so
/// tests can grep for it explicitly.
fn emit_concat_with_const_rhs(
    rhs_bytes: &[u8],
    enable_alloc_helper: bool,
) -> (String, String, String) {
    use cranelift_codegen::settings;
    use cranelift_codegen::verifier;
    use cranelift_codegen::Context;
    use relon_trace_emitter::{HostHookFuncIds, TraceEmitter};

    let mut b = TraceBuffer::new();
    let lhs = b.fresh_ssa();
    let rhs = b.fresh_ssa();
    let dst = b.fresh_ssa();
    b.append(TraceOp::ConstI64(lhs, 0x1000));
    b.append(TraceOp::ConstI64(rhs, 0x2000));
    b.record_const_bytes(rhs, rhs_bytes.to_vec());
    b.append(TraceOp::StrConcat(dst, lhs, rhs));
    b.append(TraceOp::Return(dst));

    let mut hooks = HostHookFuncIds::default();
    // `str_concat_alloc` is slotted after the historical 7-hook
    // layout (save_deopt..str_substring = 0..6). Pick FuncId 7 so
    // there's no collision with the default-layout str hooks.
    let alloc_id: u32 = 7;
    if enable_alloc_helper {
        hooks.str_concat_alloc = Some(alloc_id);
    }
    let mut ctx = Context::new();
    TraceEmitter::emit_with_hooks(
        &b.into_optimized(),
        &mut ctx,
        cranelift_codegen::ir::types::I64,
        hooks,
    )
    .expect("emit should succeed");
    let flags = settings::Flags::new(settings::builder());
    if let Err(errors) = verifier::verify_function(&ctx.func, &flags) {
        panic!(
            "cranelift verifier rejected the emitted function:\n{}\n--- IR ---\n{}",
            errors, ctx.func
        );
    }
    let ir = format!("{}", ctx.func);
    // The extern str_concat hook sits at FuncId 3 in the default
    // layout, str_concat_alloc at the supplied FuncId.
    (ir, "call fn3(".to_string(), format!("call fn{alloc_id}("))
}

#[test]
fn str_concat_inline_for_one_byte_const_rhs() {
    let (ir, extern_tag, alloc_tag) = emit_concat_with_const_rhs(b"a", true);
    assert!(
        !ir.contains(&extern_tag),
        "1-byte const rhs should bypass the extern shim:\n{ir}"
    );
    assert!(
        ir.contains(&alloc_tag),
        "inline path must call the alloc helper:\n{ir}"
    );
    // One unrolled byte store for the single rhs byte 'a' = 0x61.
    // Cranelift renders `store` without an explicit type suffix —
    // the byte width rides on the value's type — so we anchor on the
    // iconst.i8 sibling and the bare `store` instruction.
    assert!(
        ir.contains("iconst.i8 97"),
        "inline path must materialise the 'a' (0x61) const byte:\n{ir}"
    );
    assert!(
        ir.contains("store"),
        "inline path must emit a `store` for the rhs byte:\n{ir}"
    );
}

#[test]
fn str_concat_inline_for_sixteen_byte_const_rhs() {
    let (ir, extern_tag, alloc_tag) = emit_concat_with_const_rhs(b"0123456789abcdef", true);
    assert!(
        !ir.contains(&extern_tag),
        "16-byte const rhs should bypass the extern shim (boundary case):\n{ir}"
    );
    assert!(
        ir.contains(&alloc_tag),
        "inline path must call the alloc helper:\n{ir}"
    );
}

#[test]
fn str_concat_falls_back_to_extern_for_seventeen_byte_rhs() {
    let (ir, extern_tag, alloc_tag) = emit_concat_with_const_rhs(b"0123456789abcdefg", true);
    assert!(
        ir.contains(&extern_tag),
        "17-byte rhs should fall back to extern shim:\n{ir}"
    );
    assert!(
        !ir.contains(&alloc_tag),
        "17-byte rhs must NOT touch the alloc helper:\n{ir}"
    );
}

#[test]
fn str_concat_falls_back_to_extern_without_alloc_helper() {
    let (ir, extern_tag, alloc_tag) = emit_concat_with_const_rhs(b"a", false);
    assert!(
        ir.contains(&extern_tag),
        "alloc helper disabled → must keep extern shim:\n{ir}"
    );
    assert!(
        !ir.contains(&alloc_tag),
        "alloc helper disabled → emitter must not import alloc fn:\n{ir}"
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
