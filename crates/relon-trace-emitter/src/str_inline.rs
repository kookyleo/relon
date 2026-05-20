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
//! ## SIMD memchr fast path (F-D7-E)
//!
//! For needle length 1 we emit a `memchr`-style 16-byte chunked scan
//! using cranelift's portable `i8x16` ops:
//!
//! 1. `splat(I8X16, needle)` broadcasts the needle byte to all 16 lanes
//!    once before the loop.
//! 2. Each iteration loads a v128 chunk, lane-wise `icmp eq` against
//!    the splat, then `vhigh_bits → i16` reduces the lane mask to a
//!    scalar bitmask.
//! 3. `icmp_imm ne mask, 0` early-exits the loop on the first hit (any
//!    lane matched). Otherwise `cursor += 16` and re-enters.
//! 4. After the chunked loop (or immediately if `h_len < 16`) a scalar
//!    tail loop walks the remaining `h_len & 15` bytes. Same shape as
//!    the original byte-at-a-time path — three IR ops per iter.
//!
//! Cranelift lowers the v128 ops to native `pcmpeqb`/`pmovmskb` on
//! x86_64 SSE2 and `cmeq.16b`/`shrn` on aarch64 NEON. Both are the
//! standard memchr building blocks; the loop body is one load + one
//! compare + one mask-extract + one early-exit branch per 16 bytes,
//! versus 16× the same per byte in the scalar path.
//!
//! ## API selection
//!
//! A single entry point — [`emit_str_contains_inline`] — covers both
//! real use cases. The caller picks the haystack source by constructing
//! the [`HaystackHandle`] variant that matches its callsite shape:
//!
//! | Variant | Use case | Why |
//! |---|---|---|
//! | [`HaystackHandle::Raw`] | Recorder / IC lowering — haystack arrives fresh on each `StrContains` invocation in the trace stream | Loads the `(ptr, len)` payload from the `*const StringRef` SSA each call; null-checks the pointer inline. |
//! | [`HaystackHandle::Preloaded`] | Hand-built hot loop — haystack is loop-invariant and the caller wants to hoist the `StringRef` deref out of the loop | Takes a cached [`StrPayload`] (built once via [`load_string_ref_payload`]); skips per-iter `load` of `(ptr, len)`. Caller is responsible for the upstream null guard. |
//!
//! Because the variant is the decision point, the misuse mode the old
//! split-API doc warned about — calling the preloaded form with a fresh
//! per-iter haystack and leaving a hidden redundant `StringRef` load —
//! becomes a type-level mismatch instead of a doc-only footgun. F-D9 W4
//! bench measured the difference at ~6 ns / iter, enough to slip from
//! ratio ≤ × 2 to ≥ × 2.3 on a 10 KB haystack.
//!
//! ## Caller contract
//!
//! [`emit_str_contains_inline`] takes a [`HaystackHandle`] and a needle
//! byte slice, and returns the i32 result value (0 = miss, 1 = hit).
//! It must be called inside a sealed builder; on entry the current
//! cranelift block is the one that flows into the lowering, on exit the
//! current block is the join after the scan (so subsequent ops continue
//! in straight line).

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I16, I32, I64, I8X16};
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
/// [`emit_str_contains_inline`] via [`HaystackHandle::Preloaded`].
#[derive(Clone, Copy)]
pub struct StrPayload {
    pub ptr: ir::Value,
    pub len: ir::Value,
}

/// Haystack source for [`emit_str_contains_inline`]. The variant
/// chosen at the callsite encodes whether the haystack changes
/// per call (recorder / IC lowering) or is loop-invariant
/// (hand-built hot loops that hoist the `StringRef` deref).
///
/// See the module-level "API selection" table for the full
/// rationale.
pub enum HaystackHandle {
    /// `*const StringRef` raw pointer SSA value; the emitter loads
    /// `(ptr, len)` from it on entry and emits a null-pointer guard.
    /// Use when the haystack changes per call (recorder / IC lowering).
    Raw(ir::Value),
    /// Pre-loaded `(ptr, len)` payload; the emitter skips the per-call
    /// dereference. Use when the haystack is loop-invariant (hand-built
    /// hot loops that hoist `load_string_ref_payload` outside the loop).
    /// Caller is responsible for the upstream null guard.
    Preloaded(StrPayload),
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
/// `builder`. Returns the i32 result value (0 = miss, 1 = hit).
///
/// `haystack` selects how the `(ptr, len)` payload is obtained — see
/// [`HaystackHandle`] for the two variants and their use cases. The
/// variant encoding makes the recorder-vs-hoisted decision a
/// compile-time discriminant instead of a doc-level convention: pick
/// [`HaystackHandle::Raw`] for dynamic per-call haystacks (the emitter
/// loads `(ptr, len)` and null-checks inline), or
/// [`HaystackHandle::Preloaded`] for loop-invariant haystacks the
/// caller already deref'd via [`load_string_ref_payload`] (the emitter
/// skips the per-call load; the caller owns the upstream null guard).
///
/// The current block must be sealed-or-open with a single predecessor
/// path that flows here; the emitter switches blocks as part of the
/// scan and leaves the builder positioned on a freshly-sealed join
/// block when this function returns. The returned value is a
/// block-param of the join block.
pub fn emit_str_contains_inline(
    builder: &mut FunctionBuilder<'_>,
    haystack: HaystackHandle,
    needle: &[u8],
) -> ir::Value {
    match haystack {
        HaystackHandle::Raw(ptr) => emit_inline_with_raw(builder, ptr, needle),
        HaystackHandle::Preloaded(payload) => emit_inline_with_payload(builder, payload, needle),
    }
}

/// Raw-pointer entry: deref the `*const StringRef`, null-check inline,
/// then run the scan. See [`HaystackHandle::Raw`].
fn emit_inline_with_raw(
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
    // loop-invariant haystack pointer should pass `HaystackHandle::Preloaded`
    // with a hoisted `StrPayload`. This raw entrypoint is used by traces
    // where the haystack changes per-call (no obvious hoist).
    let payload = load_string_ref_payload(builder, haystack);
    emit_scan_preloaded(builder, payload, needle, join_block);

    builder.switch_to_block(join_block);
    builder.seal_block(join_block);
    builder.block_params(join_block)[0]
}

/// Preloaded-payload entry: scan directly with the cached `(ptr, len)`,
/// no per-call deref. See [`HaystackHandle::Preloaded`].
///
/// Null-haystack handling is the caller's responsibility (an upstream
/// `Guard(NotNull(haystack))` is sufficient); this variant assumes the
/// `StrPayload` is valid because the caller had to dereference the
/// pointer to load it.
fn emit_inline_with_payload(
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

/// F-D7-E single-byte SIMD specialisation. `memchr`-style scan with a
/// 16-byte chunked v128 fast path followed by a scalar tail.
///
/// ## Control flow
///
/// ```text
///     entry:
///         end_ptr   = h_ptr + h_len
///         tail_len  = h_len & 15
///         chunk_end = end_ptr - tail_len           // = h_ptr + (h_len & !15)
///         needle_v8 = splat(I8X16, needle_byte)
///         jump simd_header(h_ptr)
///
///     simd_header(cursor):
///         if cursor == chunk_end: jump tail_header(cursor)
///         chunk = load I8X16 [cursor]
///         eq    = icmp_eq chunk, needle_v8         // i8x16 lane mask
///         mask  = vhigh_bits I16, eq               // i16 top-bit-per-lane
///         if mask != 0: jump join(result = 1)
///         jump simd_header(cursor + 16)
///
///     tail_header(cursor):
///         if cursor == end_ptr: jump join(result = 0)
///         byte = load I8 [cursor]
///         if byte == needle: jump join(result = 1)
///         jump tail_header(cursor + 1)
/// ```
///
/// Cranelift lowers `icmp eq` on `i8x16` + `vhigh_bits` to
/// `pcmpeqb` + `pmovmskb` on x86_64 SSE2 (one 16-byte compare for the
/// price of one scalar one) and `cmeq.16b` + `shrn.8b` + `fmov` on
/// aarch64 NEON. On haystacks ≥ 16 bytes this is the standard memchr
/// shape; on shorter haystacks the SIMD loop is entered with
/// `cursor == chunk_end` and falls straight through to the scalar tail
/// without ever loading a v128 (no risk of out-of-bounds read).
fn emit_scan_single_byte(
    builder: &mut FunctionBuilder<'_>,
    h_ptr: ir::Value,
    h_len: ir::Value,
    needle_byte: u8,
    join_block: ir::Block,
) {
    // ----- Pre-loop constants -----
    let end_ptr = builder.ins().iadd(h_ptr, h_len);
    // `h_len & 15` — number of bytes left after the last full 16-byte
    // chunk. `chunk_end = end_ptr - tail_len = h_ptr + (h_len & !15)`.
    let tail_len = builder.ins().band_imm(h_len, 15);
    let chunk_end = builder.ins().isub(end_ptr, tail_len);

    // Splat the needle byte to all 16 lanes once. `iconst(I8, n)` then
    // `splat(I8X16, ...)` is the portable cranelift idiom; the backend
    // lowers it to a register broadcast (`pshufb`/`vbroadcastb`/etc.)
    // hoisted out of the inner loop.
    let needle_v_i8 = builder.ins().iconst(ir::types::I8, i64::from(needle_byte));
    let needle_splat = builder.ins().splat(I8X16, needle_v_i8);
    let needle_v_scalar = needle_v_i8; // reused below for the scalar tail

    // ----- SIMD 16-byte chunk loop -----
    let simd_header = builder.create_block();
    builder.append_block_param(simd_header, I64); // cursor
    builder.ins().jump(simd_header, &[BlockArg::Value(h_ptr)]);
    builder.switch_to_block(simd_header);
    let cursor_simd = builder.block_params(simd_header)[0];

    let at_chunk_end = builder.ins().icmp(IntCC::Equal, cursor_simd, chunk_end);
    let simd_body = builder.create_block();
    let tail_header = builder.create_block();
    builder.append_block_param(tail_header, I64); // cursor
    builder.ins().brif(
        at_chunk_end,
        tail_header,
        &[BlockArg::Value(cursor_simd)],
        simd_body,
        &[],
    );
    builder.seal_block(simd_body);
    builder.switch_to_block(simd_body);

    // Lane-wise compare → bitmask → early exit on any match.
    let chunk = builder
        .ins()
        .load(I8X16, MemFlags::trusted(), cursor_simd, 0);
    let eq_lanes = builder.ins().icmp(IntCC::Equal, chunk, needle_splat);
    let mask = builder.ins().vhigh_bits(I16, eq_lanes);
    let any_hit = builder.ins().icmp_imm(IntCC::NotEqual, mask, 0);

    let simd_next = builder.create_block();
    let hit_arg_simd = builder.ins().iconst(I32, 1);
    builder.ins().brif(
        any_hit,
        join_block,
        &[BlockArg::Value(hit_arg_simd)],
        simd_next,
        &[],
    );
    builder.seal_block(simd_next);
    builder.switch_to_block(simd_next);
    let sixteen = builder.ins().iconst(I64, 16);
    let next_cursor_simd = builder.ins().iadd(cursor_simd, sixteen);
    builder
        .ins()
        .jump(simd_header, &[BlockArg::Value(next_cursor_simd)]);
    builder.seal_block(simd_header);

    // ----- Scalar tail loop (≤ 15 bytes) -----
    builder.switch_to_block(tail_header);
    let cursor_tail = builder.block_params(tail_header)[0];

    let at_end = builder.ins().icmp(IntCC::Equal, cursor_tail, end_ptr);
    let tail_body = builder.create_block();
    let miss_arg = builder.ins().iconst(I32, 0);
    builder.ins().brif(
        at_end,
        join_block,
        &[BlockArg::Value(miss_arg)],
        tail_body,
        &[],
    );
    builder.seal_block(tail_body);
    builder.switch_to_block(tail_body);

    let byte = builder
        .ins()
        .load(ir::types::I8, MemFlags::trusted(), cursor_tail, 0);
    let eq = builder.ins().icmp(IntCC::Equal, byte, needle_v_scalar);

    let tail_next = builder.create_block();
    let hit_arg_tail = builder.ins().iconst(I32, 1);
    builder.ins().brif(
        eq,
        join_block,
        &[BlockArg::Value(hit_arg_tail)],
        tail_next,
        &[],
    );
    builder.seal_block(tail_next);
    builder.switch_to_block(tail_next);
    let one = builder.ins().iconst(I64, 1);
    let next_cursor_tail = builder.ins().iadd(cursor_tail, one);
    builder
        .ins()
        .jump(tail_header, &[BlockArg::Value(next_cursor_tail)]);
    builder.seal_block(tail_header);
}

/// Maximum length of the const rhs payload for which we emit the
/// inline `StrConcat` lowering. Sized identically to
/// [`MAX_INLINE_NEEDLE_LEN`] so a `b"a"` style short literal (W3 hot
/// loop) and a 16-byte boundary literal both land on the inline path
/// while longer payloads keep the extern shim. Picking the same bound
/// keeps the per-trace machine code small (`≤16` unrolled stores) and
/// matches the documented "small constant" envelope.
pub const MAX_INLINE_CONCAT_RHS_LEN: usize = MAX_INLINE_NEEDLE_LEN;

/// Should `TraceOp::StrConcat` be lowered inline given a known const
/// rhs payload? Returns `true` for rhs of length
/// `0..=[`MAX_INLINE_CONCAT_RHS_LEN`]`.
pub fn concat_rhs_fits_inline(rhs: &[u8]) -> bool {
    rhs.len() <= MAX_INLINE_CONCAT_RHS_LEN
}

/// F-D7-I: emit the inline `StrConcat(lhs, <const-rhs>)` lowering into
/// `builder`. Returns the i64 result value carrying the freshly-
/// allocated `*const StringRef` pointer.
///
/// `lhs` is a `*const StringRef` SSA value (i64); `rhs_bytes` carries
/// the const payload bytes (UTF-8) known at emit time. Caller is
/// responsible for the upstream `Guard(NotNull(lhs))` — the recorder's
/// `emit_str_concat` already emits one before the op, and the
/// allocator helper defends with a null-return on null lhs as a
/// backstop.
///
/// ## Strategy
///
/// The lowering replaces a `call __relon_str_concat(lhs, rhs)` (which
/// does UTF-8 validation on both operands + `String::with_capacity` +
/// double `push_str` + `Box<str>` shuffle) with:
///
/// 1. `len = load.i64 lhs + STRING_REF_LEN_OFFSET`
/// 2. `total = len + rhs.len()` (rhs.len is iconst)
/// 3. `result = call __relon_str_concat_alloc(lhs, total)` — the
///    helper alloc-and-memcpys the lhs prefix into the new payload
///    buffer (no UTF-8 work; just a `ptr::copy_nonoverlapping`).
/// 4. `buf = load.i64 result + STRING_REF_PTR_OFFSET`
/// 5. Unrolled `store.i8 buf + len + k, iconst(rhs[k])` for each
///    `k in 0..rhs.len()`. Cranelift's regalloc folds adjacent
///    stores into wider movs (`mov dword`/`mov qword`) where alignment
///    and width allow.
/// 6. `return result`.
///
/// The boundary cost shrinks from one full Rust shim call to one tiny
/// allocator call — the savings are the UTF-8 validation + `String`
/// growth heuristics + `Box<str>` re-allocation. On the W3 hot loop
/// (`acc + lit_a`, 1-byte rhs) the inline tail collapses to a single
/// `store.i8`.
///
/// ## Caller contract
///
/// `lhs` must be a non-null `*const StringRef` SSA (i64). `rhs_bytes`
/// must satisfy [`concat_rhs_fits_inline`] (`len ≤
/// MAX_INLINE_CONCAT_RHS_LEN`); the caller is responsible for routing
/// over-sized rhs back through the extern `__relon_str_concat` path.
pub fn emit_str_concat_inline_short_rhs(
    builder: &mut FunctionBuilder<'_>,
    str_concat_alloc: ir::FuncRef,
    lhs: ir::Value,
    rhs_bytes: &[u8],
) -> ir::Value {
    debug_assert!(
        rhs_bytes.len() <= MAX_INLINE_CONCAT_RHS_LEN,
        "rhs over inline cap; caller must fall back to extern shim"
    );
    // 1. Load lhs.len from the `*const StringRef` header.
    let lhs_len = builder.ins().load(
        I64,
        MemFlags::trusted(),
        lhs,
        relon_trace_jit::runtime::STRING_REF_LEN_OFFSET,
    );

    // 2. total_len = lhs.len + rhs.len(const).
    let rhs_len_v = builder.ins().iconst(I64, rhs_bytes.len() as i64);
    let total_len = builder.ins().iadd(lhs_len, rhs_len_v);

    // 3. Call the alloc helper: returns a fresh `*mut StringRef`
    //    whose buffer is pre-filled with the lhs prefix.
    let alloc_inst = builder.ins().call(str_concat_alloc, &[lhs, total_len]);
    let result_ptr = builder.inst_results(alloc_inst)[0];

    // 4. Load the freshly-allocated payload buffer pointer.
    let buf_ptr = builder.ins().load(
        I64,
        MemFlags::trusted(),
        result_ptr,
        relon_trace_jit::runtime::STRING_REF_PTR_OFFSET,
    );

    // 5. tail_addr = buf_ptr + lhs.len. Compute once outside the
    //    unrolled stores so cranelift's regalloc folds the address
    //    base into the per-byte store displacements.
    if !rhs_bytes.is_empty() {
        let tail_addr = builder.ins().iadd(buf_ptr, lhs_len);
        // Emit one `store.i8` per rhs byte. For W3 (rhs.len == 1) this
        // is exactly one instruction in the trace tail.
        for (k, &b) in rhs_bytes.iter().enumerate() {
            let b_v = builder.ins().iconst(ir::types::I8, i64::from(b));
            builder
                .ins()
                .store(MemFlags::trusted(), b_v, tail_addr, k as i32);
        }
    }

    // 6. Return the *mut StringRef cast back to *const StringRef
    //    (same i64 slot — opaque to the JIT).
    result_ptr
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

    #[test]
    fn concat_rhs_fits_inline_thresholds() {
        assert!(concat_rhs_fits_inline(b""));
        assert!(concat_rhs_fits_inline(b"a"));
        assert!(concat_rhs_fits_inline(b"0123456789abcdef"));
        assert!(!concat_rhs_fits_inline(b"0123456789abcdefg"));
    }
}
