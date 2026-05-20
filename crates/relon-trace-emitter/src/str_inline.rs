//! F-D7-C — inline cranelift IR for `TraceOp::StrContains` when the
//! needle bytes are known at emit time (small constant ≤ 16 bytes).
//!
//! The default lowering for `TraceOp::StrContains` is a direct
//! `call __relon_str_contains` round-trip (see
//! `crate::emitter::emit_str_contains`). For W4-shaped traces — short
//! literal needle, hot per-iteration probe — the C ABI crossing is the
//! dominant cost. This module emits the byte-scan inline so the trace
//! body stays in straight-line cranelift IR with no extern call,
//! shaving a stack-frame setup and the `__relon_str_contains` IC
//! lookup on every iter.
//!
//! ## Strategy
//!
//! Reads the `(ptr, len)` payload from the `*const StringRef` haystack
//! pointer, then unrolls a byte-by-byte scan for each candidate start
//! position. For needle length `m` and haystack length `h`, the loop
//! does at most `h - m + 1` candidate positions, each comparing `m`
//! bytes. Cranelift inlines the m-byte compare so the body is fully
//! straight-line (no inner loop). The outer loop is a `block` with two
//! params: `(idx: i64, hits: i32)`; on each iter it materialises one
//! candidate-position match, ORs the bit into `hits`, increments `idx`,
//! and re-enters until `idx > h - m`.
//!
//! Empty needle (m == 0) and m > h cases are short-circuited at emit
//! time:
//! - m == 0: result = 1 (Rust `str::contains("")` is true, even for
//!   the empty haystack).
//! - m > 0 and haystack len < m: emitted as a single `iconst.i32 0`.
//!
//! Haystack null pointer is treated as miss (i32 0); the recorder is
//! expected to emit a `Guard(NotNull(haystack))` upstream but we keep
//! the inline path safe-by-default.
//!
//! ## Why scalar, not SIMD
//!
//! Cranelift's `i8x16` lanewise compare is appealing for needle
//! length 1, but cranelift 0.131 lacks a stable mask-to-scalar idiom
//! that's portable across x86_64 + aarch64 + native-target settings the
//! bench runs in. The scalar path is enough to drop the W4 ratio
//! comfortably under 2× LuaJIT; a v128 fast path is left as a follow-up
//! (see the stage report's "remaining todo").
//!
//! ## API selection
//!
//! Two entry points cover the two real use cases. Pick by **whether the
//! haystack SSA changes per iteration**:
//!
//! | Use case | Entry point | Why |
//! |---|---|---|
//! | Recorder / IC lowering — haystack arrives fresh on each `StrContains` invocation in the trace stream | [`emit_str_contains_inline`] | Loads the `(ptr, len)` payload from the haystack each call; null-checks the pointer inline. |
//! | Hand-built hot loop — haystack is loop-invariant and the caller wants to hoist the `StringRef` deref out of the loop | [`emit_str_contains_inline_preloaded`] | Takes a cached [`StrPayload`] (built once via [`load_string_ref_payload`]); skips per-iter `load` of `(ptr, len)`. Caller is responsible for the upstream null guard. |
//!
//! Mixing them up "works" semantically but leaves the per-iter `StringRef`
//! load in place — F-D9 W4 bench measured the difference at ~6 ns / iter,
//! enough to slip from ratio ≤ × 2 to ≥ × 2.3 on a 10 KB haystack.
//!
//! ## Caller contract
//!
//! Both entry points take the haystack handle (either an i64 SSA value or
//! a pre-loaded [`StrPayload`]) and a needle byte slice, and return the i32
//! result value. They must be called inside a sealed builder; on entry the
//! current cranelift block is the one that flows into the lowering, on
//! exit the current block is the join after the scan (so subsequent ops
//! continue in straight line).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{self, BlockArg, InstBuilder, MemFlags};
use cranelift_frontend::FunctionBuilder;

/// Maximum needle length for which we emit the inline scan. Above this
/// the emitter falls back to the extern `__relon_str_contains` call to
/// keep the per-trace machine-code footprint bounded.
pub const MAX_INLINE_NEEDLE_LEN: usize = 16;

/// Should `TraceOp::StrContains` be lowered inline given a known needle?
///
/// Returns `true` for needles of length 0..=[`MAX_INLINE_NEEDLE_LEN`].
pub fn needle_fits_inline(needle: &[u8]) -> bool {
    needle.len() <= MAX_INLINE_NEEDLE_LEN
}

/// Pre-loaded `(haystack_ptr, haystack_len)` payload, as produced by
/// [`load_string_ref_payload`].
///
/// F-D7-C uses this to hoist the `StringRef` metadata load out of a
/// hot loop when the haystack SSA is loop-invariant. The bench
/// `build_w4_trace_fn` reads the W4 haystack pointer once at trace
/// entry, then passes the cached `(ptr, len)` into the per-iter
/// [`emit_str_contains_inline_preloaded`].
#[derive(Clone, Copy)]
pub struct StrPayload {
    pub ptr: ir::Value,
    pub len: ir::Value,
}

/// Load the `(ptr: *const u8, len: usize)` payload from a
/// `*const StringRef`. Both fields are returned as i64 values.
///
/// Offsets come from
/// [`relon_trace_jit::runtime::STRING_REF_PTR_OFFSET`] /
/// [`relon_trace_jit::runtime::STRING_REF_LEN_OFFSET`], which are
/// pinned to the host-side `StringRef` layout by a compile-time
/// `offset_of!` assert. Any layout drift surfaces as a build break
/// in the runtime crate, never as a silent JIT memory bug.
pub fn load_string_ref_payload(
    builder: &mut FunctionBuilder<'_>,
    string_ref_ptr: ir::Value,
) -> StrPayload {
    let ptr = builder.ins().load(
        I64,
        MemFlags::trusted(),
        string_ref_ptr,
        relon_trace_jit::runtime::STRING_REF_PTR_OFFSET,
    );
    let len = builder.ins().load(
        I64,
        MemFlags::trusted(),
        string_ref_ptr,
        relon_trace_jit::runtime::STRING_REF_LEN_OFFSET,
    );
    StrPayload { ptr, len }
}

/// Emit the inline `str_contains(haystack, needle)` lowering into
/// `builder` for a dynamic-haystack callsite (e.g. the recorder /
/// inline-cache lowering in `emitter::emit_str_contains`). Returns the
/// i32 result value (0 = miss, 1 = hit).
///
/// `haystack` is an i64 SSA carrying a `*const StringRef` pointer; the
/// pointer is dereferenced here to read `(ptr, len)`, and a null check
/// is emitted inline (null → miss).
///
/// The current block must be sealed-or-open with a single predecessor
/// path that flows here; the emitter switches blocks as part of the
/// scan and leaves the builder positioned on the join block when this
/// function returns. The returned value is a block-param of the join
/// block.
///
/// **Choose [`emit_str_contains_inline_preloaded`] instead** when the
/// haystack SSA is loop-invariant — it skips the per-iter `(ptr, len)`
/// load. See the module-level "API selection" section.
pub fn emit_str_contains_inline(
    builder: &mut FunctionBuilder<'_>,
    haystack: ir::Value,
    needle: &[u8],
) -> ir::Value {
    // Empty needle: Rust `str::contains("")` returns true even on an
    // empty haystack. Skip the scan entirely.
    if needle.is_empty() {
        return builder.ins().iconst(I32, 1);
    }

    // Null haystack → miss. Branch to the join with `result = 0`. The
    // recorder normally guards null upstream; this keeps the lowering
    // standalone-safe.
    let zero = builder.ins().iconst(I64, 0);
    let null = builder.ins().icmp(IntCC::Equal, haystack, zero);

    let nonnull_block = builder.create_block();
    let join_block = builder.create_block();
    builder.append_block_param(join_block, I32);

    let miss_arg_null = builder.ins().iconst(I32, 0);
    builder.ins().brif(
        null,
        join_block,
        &[BlockArg::Value(miss_arg_null)],
        nonnull_block,
        &[],
    );
    builder.seal_block(nonnull_block);
    builder.switch_to_block(nonnull_block);

    // Load (ptr, len) from `*haystack` and delegate to the preloaded
    // path. Cranelift 0.131 has no LICM, so a hot loop carrying a
    // loop-invariant haystack pointer should call
    // `emit_str_contains_inline_preloaded` directly with a hoisted
    // `StrPayload`. This convenience entrypoint is used by traces
    // where the haystack changes per-call (no obvious hoist).
    let payload = load_string_ref_payload(builder, haystack);
    emit_scan_preloaded(builder, payload, needle, join_block);

    builder.switch_to_block(join_block);
    builder.seal_block(join_block);
    builder.block_params(join_block)[0]
}

/// Loop-hoisted variant of [`emit_str_contains_inline`]. Takes a
/// pre-loaded `StrPayload` instead of a `*const StringRef` SSA, so the
/// caller can issue [`load_string_ref_payload`] **once** outside the
/// loop and reuse the cached `(ptr, len)` on every iteration. Used by
/// the F-D9 hand-built cmp_lua W4 trace; the recorder / IC lowering
/// uses the dynamic-haystack [`emit_str_contains_inline`] form instead.
///
/// Null-haystack handling is the caller's responsibility (an upstream
/// `Guard(NotNull(haystack))` is sufficient); this variant assumes the
/// `StrPayload` is valid because the caller had to dereference the
/// pointer to load it.
///
/// On entry the builder must be on a sealed-or-open block; on exit it
/// is positioned at a freshly-sealed join block whose sole param is
/// the i32 0/1 result.
pub fn emit_str_contains_inline_preloaded(
    builder: &mut FunctionBuilder<'_>,
    haystack: StrPayload,
    needle: &[u8],
) -> ir::Value {
    if needle.is_empty() {
        return builder.ins().iconst(I32, 1);
    }
    let join_block = builder.create_block();
    builder.append_block_param(join_block, I32);
    emit_scan_preloaded(builder, haystack, needle, join_block);
    builder.switch_to_block(join_block);
    builder.seal_block(join_block);
    builder.block_params(join_block)[0]
}

/// Internal: emit the byte-scan body assuming `needle` is non-empty
/// and the caller has already established `join_block` with one i32
/// block-param. Terminates the current path by jumping to `join_block`
/// with the i32 result.
fn emit_scan_preloaded(
    builder: &mut FunctionBuilder<'_>,
    haystack: StrPayload,
    needle: &[u8],
    join_block: ir::Block,
) {
    debug_assert!(!needle.is_empty());

    let h_ptr = haystack.ptr;
    let h_len = haystack.len;

    // F-D7-C: single-byte needle is a hot specialisation (W4 cmp_lua
    // bench). Skip the candidate-position machinery — the
    // `last_start = h_len - 1` math, the m-byte AND chain, and the
    // per-iter `iadd cand_addr` — and emit a tight `memchr`-style scan
    // that compares the haystack byte directly to the needle constant.
    if needle.len() == 1 {
        emit_scan_single_byte(builder, h_ptr, h_len, needle[0], join_block);
        return;
    }

    let m_len = needle.len() as i64;
    let m_len_v = builder.ins().iconst(I64, m_len);

    // Early exit: h_len < m_len → miss.
    let too_short = builder.ins().icmp(IntCC::UnsignedLessThan, h_len, m_len_v);
    let scan_entry = builder.create_block();
    let miss_arg_short = builder.ins().iconst(I32, 0);
    builder.ins().brif(
        too_short,
        join_block,
        &[BlockArg::Value(miss_arg_short)],
        scan_entry,
        &[],
    );
    builder.seal_block(scan_entry);
    builder.switch_to_block(scan_entry);

    // `last_start = h_len - m_len` (i.e. the inclusive upper bound on
    // the candidate start index). Computed once.
    let last_start = builder.ins().isub(h_len, m_len_v);

    // Loop header carrying only `idx`. Early-exit on match by jumping
    // directly to the join block with `result=1`; on i-out-of-range
    // jump with `result=0`. This is tighter than the original
    // `hits` accumulator because the per-iter test only checks the
    // range bound, halving the conditional work.
    let loop_header = builder.create_block();
    builder.append_block_param(loop_header, I64); // idx
    let idx_seed = builder.ins().iconst(I64, 0);
    builder
        .ins()
        .jump(loop_header, &[BlockArg::Value(idx_seed)]);
    builder.switch_to_block(loop_header);
    let idx = builder.block_params(loop_header)[0];

    let in_range = builder
        .ins()
        .icmp(IntCC::SignedLessThanOrEqual, idx, last_start);

    let body = builder.create_block();
    let miss_arg_exhausted = builder.ins().iconst(I32, 0);
    builder.ins().brif(
        in_range,
        body,
        &[],
        join_block,
        &[BlockArg::Value(miss_arg_exhausted)],
    );
    builder.seal_block(body);
    builder.switch_to_block(body);

    // Body: compute candidate match at position `idx`. AND all m byte
    // equalities into a single i8 0/1 bit. We keep the accumulator at
    // i8 width so cranelift doesn't widen each compare back to i32 on
    // each iteration — uextend lives outside the loop.
    let cand_addr = builder.ins().iadd(h_ptr, idx);
    let mut match_acc: Option<ir::Value> = None;
    for (k, &nb) in needle.iter().enumerate() {
        let byte = builder
            .ins()
            .load(ir::types::I8, MemFlags::trusted(), cand_addr, k as i32);
        let nb_v = builder.ins().iconst(ir::types::I8, i64::from(nb));
        let eq_b1 = builder.ins().icmp(IntCC::Equal, byte, nb_v);
        // cranelift's `icmp` returns an i8-typed boolean; we band the
        // raw bools to keep the accumulator one byte wide.
        match_acc = Some(match match_acc {
            None => eq_b1,
            Some(prev) => builder.ins().band(prev, eq_b1),
        });
    }
    let this_match = match_acc.expect("non-empty needle has at least one byte");

    // On match: jump directly to join_block with result=1. Otherwise
    // increment idx and loop.
    let one = builder.ins().iconst(I64, 1);
    let next_idx = builder.ins().iadd(idx, one);
    let next_iter = builder.create_block();
    let hit_arg = builder.ins().iconst(I32, 1);
    builder.ins().brif(
        this_match,
        join_block,
        &[BlockArg::Value(hit_arg)],
        next_iter,
        &[],
    );
    builder.seal_block(next_iter);
    builder.switch_to_block(next_iter);
    builder
        .ins()
        .jump(loop_header, &[BlockArg::Value(next_idx)]);
    builder.seal_block(loop_header);
}

/// F-D7-C single-byte specialisation. `memchr`-style scan: walk the
/// haystack one byte at a time, exit on the first match or when the
/// `end_ptr` is reached. Three IR ops per inner iter (load + icmp +
/// brif) plus one iadd at the bottom — half the general-case body
/// because there's no candidate-position machinery to maintain.
fn emit_scan_single_byte(
    builder: &mut FunctionBuilder<'_>,
    h_ptr: ir::Value,
    h_len: ir::Value,
    needle_byte: u8,
    join_block: ir::Block,
) {
    // Compute end pointer once. `end_ptr = h_ptr + h_len`. The loop
    // terminates when `cursor == end_ptr`.
    let end_ptr = builder.ins().iadd(h_ptr, h_len);
    let needle_v = builder.ins().iconst(ir::types::I8, i64::from(needle_byte));

    let loop_header = builder.create_block();
    builder.append_block_param(loop_header, I64); // cursor
    builder.ins().jump(loop_header, &[BlockArg::Value(h_ptr)]);
    builder.switch_to_block(loop_header);
    let cursor = builder.block_params(loop_header)[0];

    let at_end = builder.ins().icmp(IntCC::Equal, cursor, end_ptr);

    let body = builder.create_block();
    let miss_arg = builder.ins().iconst(I32, 0);
    builder
        .ins()
        .brif(at_end, join_block, &[BlockArg::Value(miss_arg)], body, &[]);
    builder.seal_block(body);
    builder.switch_to_block(body);

    let byte = builder
        .ins()
        .load(ir::types::I8, MemFlags::trusted(), cursor, 0);
    let eq = builder.ins().icmp(IntCC::Equal, byte, needle_v);

    let next_iter = builder.create_block();
    let hit_arg = builder.ins().iconst(I32, 1);
    builder
        .ins()
        .brif(eq, join_block, &[BlockArg::Value(hit_arg)], next_iter, &[]);
    builder.seal_block(next_iter);
    builder.switch_to_block(next_iter);
    let one = builder.ins().iconst(I64, 1);
    let next_cursor = builder.ins().iadd(cursor, one);
    builder
        .ins()
        .jump(loop_header, &[BlockArg::Value(next_cursor)]);
    builder.seal_block(loop_header);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needle_fits_inline_thresholds() {
        assert!(needle_fits_inline(b""));
        assert!(needle_fits_inline(b"x"));
        assert!(needle_fits_inline(b"01234567"));
        assert!(needle_fits_inline(b"0123456789abcdef"));
        // 17 bytes → fall back to extern.
        assert!(!needle_fits_inline(b"0123456789abcdefg"));
    }
}
