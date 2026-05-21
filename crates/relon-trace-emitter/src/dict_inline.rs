//! F-D8-E.4 — inline cranelift IR for `TraceOp::DictLookupPrechecked`.
//!
//! Default lowering of `TraceOp::DictLookupPrechecked` is a direct
//! `call __relon_trace_dict_lookup_prechecked(dict_ptr, key_ptr, ctx)`
//! (see [`crate::emitter::TraceEmitterState::emit_dict_lookup_prechecked`]).
//! For the W5 cmp_lua hot loop — fixed dict, short string keys, ~10
//! entries — the C ABI crossing alone runs ~6-7 ns/iter. On top of
//! that, the helper does a fresh FxHash on the key bytes and a linear
//! scan of the entry table every call. Both are cheap per iteration
//! but together they account for the bulk of the per-iter cost the
//! W5 row is measuring.
//!
//! This module emits the helper body straight into the caller's
//! cranelift IR so the trace stays in one straight-line cranelift
//! function — no extern call, no stack-frame setup, and the same
//! optimizer (cranelift's `simple_gvn` + register allocator) gets to
//! schedule the inline-cache scan against the surrounding trace ops.
//!
//! ## Layout contract
//!
//! Matches `relon-trace-jit::runtime::dict_list::build_dict_record`:
//!
//! ```text
//! offset 0  : shape_hash  : u64 LE   (already verified by DictShapeGuard)
//! offset 8  : entry_count : u32 LE
//! offset 12 : entries[0..entry_count], 16 bytes each:
//!               offset +0 : key_hash : u64 LE
//!               offset +8 : value    : i64 LE
//! ```
//!
//! Key record (the value supplied as `key_ptr`):
//!
//! ```text
//! offset 0  : len  : u32 LE   (key byte count)
//! offset 4  : bytes[0..len]   (utf8 payload)
//! ```
//!
//! ## Hash algorithm — must match `relon_trace_abi::hash::fx_hash_bytes`
//!
//! ```text
//! h = SEED
//! for &b in bytes:
//!     h ^= b as u64
//!     h = h.wrapping_mul(PRIME)
//! ```
//!
//! with `SEED = 0xcbf2_9ce4_8422_2325` and
//! `PRIME = 0x0100_0000_01b3`. Implementing the algorithm a second
//! time in IR is mandatory because the dict's entry table was
//! pre-hashed with the same constants at fixture-build / recorder
//! time — any drift would silently turn every IC lookup into a deopt.
//!
//! ## Generated control flow
//!
//! ```text
//!     entry:
//!         (key_ptr, dict_ptr supplied as SSA i64)
//!         null-check key_ptr → deopt if null
//!         key_len    = load.u32 [key_ptr + 0]    (uextend to i64)
//!         key_bytes  = key_ptr + 4
//!         jump hash_loop(byte_idx = 0, h = SEED)
//!
//!     hash_loop(byte_idx, h):
//!         done = icmp_eq byte_idx, key_len
//!         brif done, scan_init, hash_body(byte_idx, h)
//!     hash_body(byte_idx, h):
//!         b      = load.u8 [key_bytes + byte_idx]
//!         b_u64  = uextend b
//!         h1     = bxor h, b_u64
//!         h2     = imul h1, PRIME
//!         byte_idx' = byte_idx + 1
//!         jump hash_loop(byte_idx', h2)
//!
//!     scan_init:
//!         entry_count = load.u32 [dict_ptr + 8]  (uextend to i64)
//!         entries_base = dict_ptr + 12
//!         jump scan_loop(scan_idx = 0)
//!
//!     scan_loop(scan_idx):
//!         exhausted = icmp_eq scan_idx, entry_count
//!         brif exhausted, deopt(0, 0), scan_body
//!     scan_body:
//!         entry_off  = scan_idx * 16
//!         entry_addr = entries_base + entry_off
//!         entry_hash = load.u64 [entry_addr + 0]
//!         is_hit     = icmp_eq entry_hash, h
//!         brif is_hit, hit_block, scan_next
//!     scan_next:
//!         scan_idx'  = scan_idx + 1
//!         jump scan_loop(scan_idx')
//!     hit_block:
//!         value      = load.i64 [entry_addr + 8]
//!         jump join(value)
//!
//!     join(value):
//!         bind dst → value
//! ```
//!
//! The `deopt` block is the trace-emitter's shared deopt sink. We
//! reuse it on key-miss (and on null `key_ptr`) so a runaway dict
//! mutation since recorder time deopts the same way the helper would
//! have.
//!
//! ## Why no upper bound on `entry_count` here
//!
//! The caller is responsible for choosing inline vs helper. For W5
//! `entry_count == 10`; we still emit the inline scan even for larger
//! tables because the inner loop is tight (one load + one compare +
//! one brif per entry). The
//! [`crate::emitter::TraceEmitterState::emit_dict_lookup_prechecked`]
//! dispatcher applies a soft cap so machine-code footprint stays
//! bounded; see that callsite for the constant.
//!
//! ## Inline / fallback decision pattern
//!
//! `DictLookupPrechecked` (and its sibling `StrContains` in
//! [`crate::str_inline`]) share a three-tier dispatch pattern, run
//! by the emitter at op-lowering time:
//!
//! 1. **Probe side table** — look up the recorder's per-SSA hint
//!    ([`relon_trace_jit::OptimizedTrace::dict_entry_count_hint`]
//!    for dicts; `const_bytes_for` for strings). Absence means
//!    "no hint" — take the most general path.
//! 2. **Threshold check** — when a hint is present, compare it
//!    against the inline-form cap ([`MAX_INLINE_UNROLL`] for
//!    dict; [`crate::str_inline::MAX_INLINE_NEEDLE_LEN`] for str).
//!    Above the cap, fall through to the next tier — the inline
//!    form's machine-code footprint would dominate the win.
//! 3. **Lowering tier** — pick from (best → worst):
//!    - **Fully inline / unrolled** — straight-line cranelift IR
//!      with no extern call and no inner loop. Used only when
//!      both (1) and (2) hold.
//!    - **Inline data-driven** — straight-line cranelift IR with
//!      a tight inner loop. Used for dicts that have a hint above
//!      the unroll cap, and for strs the recorder pinned a needle
//!      but it's > `MAX_INLINE_NEEDLE_LEN` (today: falls through
//!      to extern instead — no intermediate tier for str).
//!    - **Extern shim call** — fallback when no hint is recorded.
//!
//! The dispatcher lives in [`crate::emitter`] rather than this
//! module because the per-callsite glue (SSA lookups, hoisted-SSA
//! bundles, deopt-block plumbing) is emitter state. The inline
//! modules export the cap constants and the inline-form emitters;
//! the emitter owns the if-let chain that walks the tiers. A
//! generic `InlineDecisionHelper<T>` abstraction was considered
//! and rejected — the dict and str dispatchers branch on different
//! key types (`u32` count vs `&[u8]` payload) and feed disjoint
//! inline-form signatures, so the helper would collapse to two
//! independent callsites masquerading as one.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64, I8};
use cranelift_codegen::ir::{self, BlockArg, InstBuilder, MemFlags};
use cranelift_frontend::FunctionBuilder;

/// FxHash64 seed — must match
/// `relon_trace_abi::hash::fx_hash_bytes`.
const FX_HASH_SEED: i64 = 0xcbf2_9ce4_8422_2325u64 as i64;
/// FxHash64 prime multiplier — must match
/// `relon_trace_abi::hash::fx_hash_bytes`.
const FX_HASH_PRIME: i64 = 0x0100_0000_01b3u64 as i64;

/// F-D8-E.7: getter for the FxHash seed value so emitter-side
/// preheader hoist code can stamp the same i64 constant without
/// re-importing the private const. Returning a plain `i64` keeps the
/// constant `const fn`-callable from any module.
#[inline]
pub const fn fx_hash_seed_i64() -> i64 {
    FX_HASH_SEED
}

/// F-D8-E.7: getter for the FxHash prime multiplier value. See
/// [`fx_hash_seed_i64`] for the rationale.
#[inline]
pub const fn fx_hash_prime_i64() -> i64 {
    FX_HASH_PRIME
}

/// Soft cap on `entry_count` for the inline form. Above this the
/// emitter should fall back to the helper call so the per-trace
/// machine-code footprint stays bounded. (The IR shape is constant
/// in `entry_count` — the loop is data-driven — so this is purely a
/// guard against arbitrarily-large dicts.) The cap is generous: W5
/// has 10 entries, and even a 64-entry table compiles to a single
/// tight loop with one load + one cmp + one brif per entry.
pub const MAX_INLINE_ENTRY_HINT: u32 = 64;

/// F-D8-E.7: cap on `entry_count` for the **fully-unrolled** inline
/// form. Above this we fall back to the data-driven scan loop.
///
/// Empirical perf study (W5 cmp_lua, 10-entry dict, round-robin key
/// access): unrolling to N=10 makes every outer-loop iteration do N
/// loads + N icmps + N selects + ~2N-1 bors, regardless of where the
/// hit lands. The original scan loop terminates at the hit position,
/// so for uniform round-robin its average work is ~N/2 entries per
/// iter; that average wins on W5's 10-entry distribution even after
/// the scan-loop's branch-misprediction cost. The unrolled path is
/// faster when:
///   (a) N is small enough that the difference between N and N/2 is
///       a single load + cmp pair (i.e. N <= 4), or
///   (b) the recorded access pattern hits the first 1-2 entries
///       overwhelmingly (then the scan loop wastes branch-predictor
///       bandwidth re-learning the same prediction).
/// At N >= 8 with round-robin access the unrolled form regresses by
/// ~70% on the W5 fixture. Capping the trigger at 4 means small
/// dicts (e.g. 2-3 entry config structs) get the unroll win while
/// W5-class loops keep the scan path.
pub const MAX_INLINE_UNROLL: u32 = 4;

/// Emit the inline body of `__relon_trace_dict_lookup_prechecked`
/// directly into `builder`. The caller has already verified the
/// dict's shape via a paired `DictShapeGuard`; we skip the leading
/// `dict_shape != shape_hash` compare and start straight at the key
/// hash.
///
/// On a successful lookup the i64 dict value is returned. On a key
/// miss or null `key_ptr` the emitter jumps to `deopt_block` with
/// `(guard_pc=0, external_pc=0)` — same sentinel the helper-call
/// lowering uses on the matching deopt arm.
///
/// `deopt_block` must take two block params of types `(I32, I64)`
/// (matching the shared trace deopt convention; see
/// `TraceEmitterState::deopt_block` setup).
///
/// Caller contract: this function switches blocks as part of its
/// lowering and leaves the builder positioned on a freshly-sealed
/// join block. The returned value is a block-param of the join.
///
/// Uses an `acc_h`-carrying join block from the hash loop into the
/// scan loop, which keeps the IR purely SSA without resorting to
/// stack slots.
pub fn emit_dict_lookup_inline(
    builder: &mut FunctionBuilder<'_>,
    dict_ptr: ir::Value,
    key_ptr: ir::Value,
    deopt_block: ir::Block,
) -> ir::Value {
    emit_dict_lookup_inline_with_entry_count(builder, dict_ptr, key_ptr, deopt_block, None)
}

/// F-D8-E.7: bundle of optional preheader-hoisted SSAs the inline
/// scan body can reuse instead of re-emitting per outer-loop
/// iteration. Each field is `None` when the caller didn't hoist that
/// particular subexpression; the inline body then falls back to its
/// per-iter emit. The struct exists so the emitter can grow new
/// hoisted fields (e.g. `hash_seed`, `hash_prime`, `entries_base`)
/// without churning the public function signatures every time.
#[derive(Debug, Default, Clone, Copy)]
pub struct DictInlineHoists {
    /// i64 SSA holding the dict's `entry_count` (`load.u32 +
    /// uextend`). See F-D8-E.5.
    pub entry_count: Option<ir::Value>,
    /// i64 SSA holding the FxHash seed (`iconst.i64 FX_HASH_SEED`).
    /// Hoisting deletes one 10-byte `mov reg, imm64` from the
    /// per-outer-iter hash-loop preamble.
    pub hash_seed: Option<ir::Value>,
    /// i64 SSA holding the FxHash prime multiplier
    /// (`iconst.i64 FX_HASH_PRIME`). Hoisting deletes one 10-byte
    /// `mov reg, imm64` from each hash-body iteration — saves
    /// `key_len + 1` mov-imm64 per outer iter (W5 has 1-byte keys
    /// → 2 saves per iter).
    pub hash_prime: Option<ir::Value>,
}

/// F-D8-E.5 variant of [`emit_dict_lookup_inline`] that accepts a
/// preheader-hoisted `entry_count` SSA. When `Some`, the inline body
/// skips the per-iter `load.u32 [dict_ptr+8] + uextend` pair and
/// feeds the cached i64 SSA straight into the scan loop's exit
/// predicate.
///
/// We deliberately do NOT hoist `entries_base = dict_ptr + 12` — the
/// scan body uses it as `scan_idx * 16 + entries_base`, an expression
/// cranelift folds into a single x86_64 `lea` with displacement when
/// the iadd_imm stays inside the body. Hoisting it as a separate SSA
/// would break that fold and net out negative on the hot path.
///
/// Functionally identical to the unparametrised entry point when
/// `entry_count` is `None`.
pub fn emit_dict_lookup_inline_with_entry_count(
    builder: &mut FunctionBuilder<'_>,
    dict_ptr: ir::Value,
    key_ptr: ir::Value,
    deopt_block: ir::Block,
    hoisted_entry_count: Option<ir::Value>,
) -> ir::Value {
    emit_dict_lookup_inline_with_hoists(
        builder,
        dict_ptr,
        key_ptr,
        deopt_block,
        DictInlineHoists {
            entry_count: hoisted_entry_count,
            hash_seed: None,
            hash_prime: None,
        },
    )
}

/// F-D8-E.7 superset of [`emit_dict_lookup_inline_with_entry_count`]:
/// accepts a [`DictInlineHoists`] bundle so the inline body can reuse
/// preheader-hoisted `entry_count`, `hash_seed`, and `hash_prime`
/// SSAs instead of re-emitting their `iconst` / `load` ops each
/// outer-loop iteration.
///
/// On x86_64 the `FX_HASH_SEED` and `FX_HASH_PRIME` constants don't
/// fit in a 32-bit immediate, so each in-body `iconst.i64 imm64`
/// lowers to a 10-byte `movabs reg, imm64` plus a long-lived
/// register reservation. Hoisting them to the preheader (where they
/// emit exactly once per trace entry) shortens the hash-body insn
/// stream and frees the registers for the hash accumulator and the
/// scan loop's pointer chase.
pub fn emit_dict_lookup_inline_with_hoists(
    builder: &mut FunctionBuilder<'_>,
    dict_ptr: ir::Value,
    key_ptr: ir::Value,
    deopt_block: ir::Block,
    hoists: DictInlineHoists,
) -> ir::Value {
    let hoisted_entry_count = hoists.entry_count;
    // ----- Null-key guard ------------------------------------------------
    let zero = builder.ins().iconst(I64, 0);
    let key_null = builder.ins().icmp(IntCC::Equal, key_ptr, zero);
    let nonnull = builder.create_block();
    let guard_pc_null = builder.ins().iconst(I32, 0);
    let ext_pc_null = builder.ins().iconst(I64, 0);
    builder.ins().brif(
        key_null,
        deopt_block,
        &[BlockArg::Value(guard_pc_null), BlockArg::Value(ext_pc_null)],
        nonnull,
        &[],
    );
    builder.seal_block(nonnull);
    builder.switch_to_block(nonnull);

    // ----- Key payload header -------------------------------------------
    let key_len_u32 = builder.ins().load(I32, MemFlags::trusted(), key_ptr, 0);
    let key_len = builder.ins().uextend(I64, key_len_u32);
    let key_bytes = builder.ins().iadd_imm(key_ptr, 4);

    // ----- Hash loop ----------------------------------------------------
    //
    // `(idx, h)` carry per-iter state. We branch the exit edge into a
    // dedicated `scan_init` block that carries the final hash as a
    // block-param, so the value flows into the scan loop as a normal
    // SSA value.
    let hash_header = builder.create_block();
    builder.append_block_param(hash_header, I64); // byte_idx
    builder.append_block_param(hash_header, I64); // accumulator h
    let zero_i64 = builder.ins().iconst(I64, 0);
    // F-D8-E.7: prefer the caller-supplied hoisted seed SSA when
    // available; otherwise materialise the seed inside the inline
    // body. The hoisted form must dominate the dict-inline call site
    // (preheader block, by construction).
    let seed_i64 = match hoists.hash_seed {
        Some(v) => v,
        None => builder.ins().iconst(I64, FX_HASH_SEED),
    };
    builder.ins().jump(
        hash_header,
        &[BlockArg::Value(zero_i64), BlockArg::Value(seed_i64)],
    );
    builder.switch_to_block(hash_header);
    let byte_idx = builder.block_params(hash_header)[0];
    let acc_h = builder.block_params(hash_header)[1];

    let hash_done = builder.ins().icmp(IntCC::Equal, byte_idx, key_len);
    let scan_init = builder.create_block();
    builder.append_block_param(scan_init, I64); // final hash
    let hash_body = builder.create_block();
    builder.ins().brif(
        hash_done,
        scan_init,
        &[BlockArg::Value(acc_h)],
        hash_body,
        &[],
    );
    builder.seal_block(hash_body);
    builder.switch_to_block(hash_body);

    let byte_addr = builder.ins().iadd(key_bytes, byte_idx);
    let b = builder.ins().load(I8, MemFlags::trusted(), byte_addr, 0);
    let b_i64 = builder.ins().uextend(I64, b);
    let xored = builder.ins().bxor(acc_h, b_i64);
    // F-D8-E.7: same hoist treatment for FX_HASH_PRIME. Hoisting the
    // 64-bit immediate avoids re-issuing a `movabs reg, imm64` on
    // every hash-body iteration; cranelift 0.131's GVN does not
    // reliably lift `iconst.i64 imm64` across loop bodies.
    let prime_v = match hoists.hash_prime {
        Some(v) => v,
        None => builder.ins().iconst(I64, FX_HASH_PRIME),
    };
    let next_h = builder.ins().imul(xored, prime_v);
    let one_i64 = builder.ins().iconst(I64, 1);
    let next_idx = builder.ins().iadd(byte_idx, one_i64);
    builder.ins().jump(
        hash_header,
        &[BlockArg::Value(next_idx), BlockArg::Value(next_h)],
    );
    builder.seal_block(hash_header);

    // ----- Scan loop ----------------------------------------------------
    builder.switch_to_block(scan_init);
    let final_hash = builder.block_params(scan_init)[0];

    // entry_count is u32 LE at dict_ptr + 8 (shape is +0, already
    // verified by DictShapeGuard); entries start at dict_ptr + 12.
    //
    // F-D8-E.5: when the caller hoisted `entry_count` out of the loop
    // (because `dict_ptr` is loop-invariant), reuse the cached SSA
    // here instead of re-issuing the load every iteration. The hoist
    // is sourced from the preheader block, which strictly dominates
    // this scan-init block, so the cranelift verifier accepts the
    // reused value as a plain input. `entries_base` stays inline so
    // the `scan_idx * 16 + dict_ptr + 12` chain folds into one `lea`.
    let entry_count = match hoisted_entry_count {
        Some(v) => v,
        None => {
            let entry_count_u32 = builder.ins().load(I32, MemFlags::trusted(), dict_ptr, 8);
            builder.ins().uextend(I64, entry_count_u32)
        }
    };
    // F-D8-E.7: switch the scan loop from "index + imul + iadd" to
    // "incremental entry_ptr". The original IR shape lowered the
    // per-iter address computation to a 3-op sequence — `imul
    // scan_idx, 16; iadd entries_base, off` — that cranelift's
    // x86_64 backend folds into a single `lea`, but the multiplier
    // chain still serialises behind the index update. By carrying
    // the entry pointer itself across the back-edge and bumping it
    // by 16 each iter, we get a single `add reg, 16` per iter and
    // free the `imul` + `iadd` slots. We precompute `entries_end =
    // entries_base + entry_count * 16` once at scan_init and the
    // header tests `entry_ptr == entries_end` for termination.
    //
    // `entries_base` and `entries_end` stay inside this scan-init
    // block (not hoisted to the preheader) so the cranelift
    // x86_64 backend folds the `iadd_imm dict_ptr, 12` into the
    // first load's displacement when `dict_ptr` is loop-invariant.
    let entries_base = builder.ins().iadd_imm(dict_ptr, 12);
    let sixteen_init = builder.ins().iconst(I64, 16);
    let total_bytes = builder.ins().imul(entry_count, sixteen_init);
    let entries_end = builder.ins().iadd(entries_base, total_bytes);
    builder.seal_block(scan_init);

    // Scan header: carries the current entry pointer.
    let scan_header = builder.create_block();
    builder.append_block_param(scan_header, I64); // entry_ptr
    builder
        .ins()
        .jump(scan_header, &[BlockArg::Value(entries_base)]);
    builder.switch_to_block(scan_header);
    let entry_ptr = builder.block_params(scan_header)[0];

    let exhausted = builder.ins().icmp(IntCC::Equal, entry_ptr, entries_end);
    let scan_body = builder.create_block();
    let guard_pc_miss = builder.ins().iconst(I32, 0);
    let ext_pc_miss = builder.ins().iconst(I64, 0);
    builder.ins().brif(
        exhausted,
        deopt_block,
        &[BlockArg::Value(guard_pc_miss), BlockArg::Value(ext_pc_miss)],
        scan_body,
        &[],
    );
    builder.seal_block(scan_body);
    builder.switch_to_block(scan_body);

    // Each entry is 16 bytes: [u64 hash][i64 value]. Read both off
    // the current `entry_ptr` (offset 0 and 8 respectively).
    let entry_hash = builder.ins().load(I64, MemFlags::trusted(), entry_ptr, 0);
    let is_hit = builder.ins().icmp(IntCC::Equal, entry_hash, final_hash);

    let hit_block = builder.create_block();
    let scan_next = builder.create_block();
    builder.ins().brif(is_hit, hit_block, &[], scan_next, &[]);
    builder.seal_block(hit_block);
    builder.seal_block(scan_next);

    // ----- Scan next ----------------------------------------------------
    builder.switch_to_block(scan_next);
    let next_ptr = builder.ins().iadd_imm(entry_ptr, 16);
    builder
        .ins()
        .jump(scan_header, &[BlockArg::Value(next_ptr)]);
    builder.seal_block(scan_header);

    // ----- Hit block ----------------------------------------------------
    builder.switch_to_block(hit_block);
    let value = builder.ins().load(I64, MemFlags::trusted(), entry_ptr, 8);

    // ----- Join ---------------------------------------------------------
    let join_block = builder.create_block();
    builder.append_block_param(join_block, I64);
    builder.ins().jump(join_block, &[BlockArg::Value(value)]);
    builder.switch_to_block(join_block);
    builder.seal_block(join_block);
    builder.block_params(join_block)[0]
}

/// F-D8-E.7 — fully-unrolled inline body of
/// `__relon_trace_dict_lookup_prechecked` for the case where the
/// caller knows the dict's entry count is statically `entry_count`
/// (`<= MAX_INLINE_UNROLL`).
///
/// Replaces the data-driven scan-loop in
/// [`emit_dict_lookup_inline_with_entry_count`] with a straight-line
/// branch-free chain of N `(load + icmp + select)` triplets. cranelift
/// 0.131's x86_64 backend lowers each `select.i64` to a single `cmov`,
/// so a 10-entry W5 dict compiles to 10 `cmov` insns instead of an
/// average ~5 loop iterations × `(load + cmp + brif + jump)` each. The
/// dict_scan stop-condition (entry exhaustion → deopt) is replaced by
/// a single `any_hit` branch at the tail: if no entry matched, deopt;
/// otherwise carry `result` into the join.
///
/// Same key-hash algorithm and same shape contract as
/// [`emit_dict_lookup_inline`] — the unrolled form only changes the
/// scan loop, not the hash loop. The hash loop stays loop-shaped
/// because cranelift can't statically unroll it (`key_len` is a
/// per-call value), but the W5 hot path's keys are 1 byte long so the
/// hash loop runs exactly 2 iterations (init + body) and shouldn't be
/// the bottleneck the unroll targets.
///
/// `entry_count` must be `>= 1` and `<= MAX_INLINE_UNROLL`; callers
/// are responsible for choosing this variant only when both conditions
/// hold (see `emitter::TraceEmitterState::emit_dict_lookup_prechecked`).
pub fn emit_dict_lookup_inline_unrolled(
    builder: &mut FunctionBuilder<'_>,
    dict_ptr: ir::Value,
    key_ptr: ir::Value,
    deopt_block: ir::Block,
    entry_count: u32,
) -> ir::Value {
    debug_assert!(entry_count >= 1);
    debug_assert!(entry_count <= MAX_INLINE_UNROLL);

    // ----- Null-key guard ------------------------------------------------
    let zero = builder.ins().iconst(I64, 0);
    let key_null = builder.ins().icmp(IntCC::Equal, key_ptr, zero);
    let nonnull = builder.create_block();
    let guard_pc_null = builder.ins().iconst(I32, 0);
    let ext_pc_null = builder.ins().iconst(I64, 0);
    builder.ins().brif(
        key_null,
        deopt_block,
        &[BlockArg::Value(guard_pc_null), BlockArg::Value(ext_pc_null)],
        nonnull,
        &[],
    );
    builder.seal_block(nonnull);
    builder.switch_to_block(nonnull);

    // ----- Key payload header -------------------------------------------
    let key_len_u32 = builder.ins().load(I32, MemFlags::trusted(), key_ptr, 0);
    let key_len = builder.ins().uextend(I64, key_len_u32);
    let key_bytes = builder.ins().iadd_imm(key_ptr, 4);

    // ----- Hash loop ----------------------------------------------------
    // Identical to the data-driven variant — `key_len` is a per-call
    // input so we can't statically unroll it here, but the loop body
    // is tight enough that cranelift can keep the accumulator hot.
    let hash_header = builder.create_block();
    builder.append_block_param(hash_header, I64); // byte_idx
    builder.append_block_param(hash_header, I64); // accumulator h
    let zero_i64 = builder.ins().iconst(I64, 0);
    let seed_i64 = builder.ins().iconst(I64, FX_HASH_SEED);
    builder.ins().jump(
        hash_header,
        &[BlockArg::Value(zero_i64), BlockArg::Value(seed_i64)],
    );
    builder.switch_to_block(hash_header);
    let byte_idx = builder.block_params(hash_header)[0];
    let acc_h = builder.block_params(hash_header)[1];

    let hash_done = builder.ins().icmp(IntCC::Equal, byte_idx, key_len);
    let scan_block = builder.create_block();
    builder.append_block_param(scan_block, I64); // final hash
    let hash_body = builder.create_block();
    builder.ins().brif(
        hash_done,
        scan_block,
        &[BlockArg::Value(acc_h)],
        hash_body,
        &[],
    );
    builder.seal_block(hash_body);
    builder.switch_to_block(hash_body);

    let byte_addr = builder.ins().iadd(key_bytes, byte_idx);
    let b = builder.ins().load(I8, MemFlags::trusted(), byte_addr, 0);
    let b_i64 = builder.ins().uextend(I64, b);
    let xored = builder.ins().bxor(acc_h, b_i64);
    let prime_v = builder.ins().iconst(I64, FX_HASH_PRIME);
    let next_h = builder.ins().imul(xored, prime_v);
    let one_i64 = builder.ins().iconst(I64, 1);
    let next_idx = builder.ins().iadd(byte_idx, one_i64);
    builder.ins().jump(
        hash_header,
        &[BlockArg::Value(next_idx), BlockArg::Value(next_h)],
    );
    builder.seal_block(hash_header);

    // ----- Unrolled scan ------------------------------------------------
    //
    // For each entry slot `k in 0..entry_count`:
    //   entry_hash_k = load.u64 [dict_ptr + 12 + k*16 + 0]
    //   entry_val_k  = load.i64 [dict_ptr + 12 + k*16 + 8]
    //   hit_k        = icmp.eq entry_hash_k, final_hash
    //   contrib_k    = select(hit_k, entry_val_k, 0)
    //
    // We then reduce the per-entry `contrib` lanes through a balanced
    // `bor` tree (depth = ceil(log2(N))) so the dependency chain
    // length is O(log N) instead of O(N). The per-entry `select` /
    // `icmp` / `load` ops are mutually independent so cranelift's
    // x86_64 backend (and the CPU's OoO window) can issue them in
    // parallel. A naive left-fold `value = select(hit_k, val_k, value)`
    // would emit a chain of N cmov insns whose data dependency forces
    // serial execution — pessimising the unroll into a slowdown.
    //
    // The same shape applies to `any_hit`: build a parallel array of
    // `hit_k` bits and reduce them via a `bor` tree.
    //
    // The `iadd_imm` per-entry stays inside this block; cranelift's
    // x86_64 displacement folding collapses
    // `load [dict_ptr + 12 + k*16 + 0]` (a 32-bit displacement) into a
    // single `mov` per load — no separate `lea` needed.
    //
    // Tie-breaking on hash collisions: the IR guarantees each dict
    // shape pins a unique key set; the `DictShapeGuard` upstream
    // verifies that pin at runtime. So at most ONE entry's hash can
    // equal `final_hash` — `or` of all `contrib_k` therefore yields
    // exactly the hit's value (or 0 on miss).
    builder.switch_to_block(scan_block);
    let final_hash = builder.block_params(scan_block)[0];
    builder.seal_block(scan_block);

    // Stage 1: independent per-entry compute lanes.
    let zero_acc = builder.ins().iconst(I64, 0);
    let mut hits: Vec<ir::Value> = Vec::with_capacity(entry_count as usize);
    let mut contribs: Vec<ir::Value> = Vec::with_capacity(entry_count as usize);
    for k in 0..entry_count {
        let base_off = 12i32 + (k as i32) * 16;
        let hash_off = base_off; // + 0
        let val_off = base_off + 8;
        let entry_hash = builder
            .ins()
            .load(I64, MemFlags::trusted(), dict_ptr, hash_off);
        let entry_val = builder
            .ins()
            .load(I64, MemFlags::trusted(), dict_ptr, val_off);
        let hit = builder.ins().icmp(IntCC::Equal, entry_hash, final_hash);
        let contrib = builder.ins().select(hit, entry_val, zero_acc);
        hits.push(hit);
        contribs.push(contrib);
    }

    // Stage 2: balanced `bor` tree reduction over `contribs` and
    // `hits`. Reduces dependency chain length from N to ceil(log2(N)).
    // For W5 (N=10) this is 4 levels.
    let value_acc = bor_tree_reduce_i64(builder, &contribs);
    // `hit` lanes are i8 (icmp result); reduce them via i8 `bor` too.
    let any_hit = bor_tree_reduce_i8(builder, &hits);

    // Tail: branch on whether any entry hit.
    let hit_block = builder.create_block();
    builder.append_block_param(hit_block, I64);
    let guard_pc_miss = builder.ins().iconst(I32, 0);
    let ext_pc_miss = builder.ins().iconst(I64, 0);
    builder.ins().brif(
        any_hit,
        hit_block,
        &[BlockArg::Value(value_acc)],
        deopt_block,
        &[BlockArg::Value(guard_pc_miss), BlockArg::Value(ext_pc_miss)],
    );
    builder.switch_to_block(hit_block);
    builder.seal_block(hit_block);
    builder.block_params(hit_block)[0]
}

/// Reduce a slice of i64 lanes through a balanced `bor` tree. The
/// caller guarantees that at most one lane is non-zero (the others
/// are all zero contribs), so `or` is equivalent to "pick the unique
/// non-zero lane (or 0 if none matched)". A balanced tree keeps the
/// dependency chain at `ceil(log2(lanes.len()))`, which is the lever
/// for unroll perf — a left-fold would serialise the whole chain.
fn bor_tree_reduce_i64(builder: &mut FunctionBuilder<'_>, lanes: &[ir::Value]) -> ir::Value {
    debug_assert!(!lanes.is_empty(), "bor_tree_reduce_i64 requires ≥1 lane");
    let mut level: Vec<ir::Value> = lanes.to_vec();
    while level.len() > 1 {
        let mut next: Vec<ir::Value> = Vec::with_capacity(level.len().div_ceil(2));
        let mut i = 0;
        while i + 1 < level.len() {
            let merged = builder.ins().bor(level[i], level[i + 1]);
            next.push(merged);
            i += 2;
        }
        if i < level.len() {
            next.push(level[i]); // odd lane carried over to next round
        }
        level = next;
    }
    level[0]
}

/// i8 variant of [`bor_tree_reduce_i64`]. Used for the `any_hit`
/// reduction over `icmp` (i8) results.
fn bor_tree_reduce_i8(builder: &mut FunctionBuilder<'_>, lanes: &[ir::Value]) -> ir::Value {
    debug_assert!(!lanes.is_empty(), "bor_tree_reduce_i8 requires ≥1 lane");
    let mut level: Vec<ir::Value> = lanes.to_vec();
    while level.len() > 1 {
        let mut next: Vec<ir::Value> = Vec::with_capacity(level.len().div_ceil(2));
        let mut i = 0;
        while i + 1 < level.len() {
            let merged = builder.ins().bor(level[i], level[i + 1]);
            next.push(merged);
            i += 2;
        }
        if i < level.len() {
            next.push(level[i]);
        }
        level = next;
    }
    level[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::ir::types::I32;
    use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, Signature, UserFuncName};
    use cranelift_codegen::isa::CallConv;
    use cranelift_codegen::settings::{self, Configurable};
    use cranelift_codegen::verifier::verify_function;
    use cranelift_frontend::FunctionBuilderContext;

    /// Build a trivial function around `emit_dict_lookup_inline` so we
    /// can verify it produces well-formed SSA.
    fn build_test_fn() -> Function {
        // (dict_ptr: i64, key_ptr: i64) -> i64
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);

        let mut bcx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut bcx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let dict_ptr = builder.block_params(entry)[0];
        let key_ptr = builder.block_params(entry)[1];

        // Build a deopt sink that returns -1 (sentinel) so the function
        // remains well-formed even on the miss path.
        let deopt = builder.create_block();
        builder.append_block_param(deopt, I32);
        builder.append_block_param(deopt, I64);
        // Body for deopt is filled later.

        let value = emit_dict_lookup_inline(&mut builder, dict_ptr, key_ptr, deopt);
        builder.ins().return_(&[value]);

        builder.switch_to_block(deopt);
        builder.seal_block(deopt);
        let sentinel = builder.ins().iconst(I64, -1);
        builder.ins().return_(&[sentinel]);

        builder.finalize();
        func
    }

    #[test]
    fn emit_produces_valid_cranelift_ir() {
        let func = build_test_fn();
        let mut flag_builder = settings::builder();
        flag_builder.set("opt_level", "speed").unwrap();
        let flags = settings::Flags::new(flag_builder);
        verify_function(&func, &flags).expect("cranelift IR must verify");
    }

    /// The inline emitter should mirror `relon_trace_abi::hash::fx_hash_bytes`
    /// for the constants — drift would silently turn IC lookups into
    /// deopts. Compare the constants directly so a refactor that
    /// shifts one side surfaces here.
    /// Build a function around `emit_dict_lookup_inline_unrolled` with
    /// a static N. We use the same wrapper shape so both inline
    /// variants can share the verifier-side smoke test.
    fn build_test_fn_unrolled(entry_count: u32) -> Function {
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(I64));
        sig.params.push(AbiParam::new(I64));
        sig.returns.push(AbiParam::new(I64));
        let mut func = Function::with_name_signature(UserFuncName::user(0, 0), sig);

        let mut bcx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut bcx);

        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let dict_ptr = builder.block_params(entry)[0];
        let key_ptr = builder.block_params(entry)[1];

        let deopt = builder.create_block();
        builder.append_block_param(deopt, I32);
        builder.append_block_param(deopt, I64);

        let value =
            emit_dict_lookup_inline_unrolled(&mut builder, dict_ptr, key_ptr, deopt, entry_count);
        builder.ins().return_(&[value]);

        builder.switch_to_block(deopt);
        builder.seal_block(deopt);
        let sentinel = builder.ins().iconst(I64, -1);
        builder.ins().return_(&[sentinel]);

        builder.finalize();
        func
    }

    /// F-D8-E.7: the fully-unrolled inline form must produce well-
    /// formed cranelift IR for both small (W5 = 10) and the corner
    /// cases at the edges of `MAX_INLINE_UNROLL`.
    #[test]
    fn emit_unrolled_produces_valid_cranelift_ir_for_n_1_to_max() {
        let mut flag_builder = settings::builder();
        flag_builder.set("opt_level", "speed").unwrap();
        let flags = settings::Flags::new(flag_builder);
        // Cover the supported `entry_count` range up through the
        // MAX_INLINE_UNROLL cap inclusive. The fixture loop will
        // grow / shrink alongside the cap if a future tuning round
        // adjusts it.
        for n in [
            1u32,
            2,
            MAX_INLINE_UNROLL.saturating_sub(1).max(1),
            MAX_INLINE_UNROLL,
        ] {
            let func = build_test_fn_unrolled(n);
            verify_function(&func, &flags)
                .unwrap_or_else(|e| panic!("unrolled IR (n={n}) must verify: {e:?}"));
        }
    }

    /// F-D8-E.7: the unrolled form must contain N `select` insns —
    /// one per entry slot in the cmov chain. Walk all insts via the
    /// layout's block iteration and count `Opcode::Select`.
    #[test]
    fn emit_unrolled_emits_n_select_insns() {
        let n = 4u32;
        let func = build_test_fn_unrolled(n);
        let mut select_count = 0usize;
        for block in func.layout.blocks() {
            for inst in func.layout.block_insts(block) {
                if func.dfg.insts[inst].opcode() == cranelift_codegen::ir::Opcode::Select {
                    select_count += 1;
                }
            }
        }
        assert_eq!(
            select_count, n as usize,
            "unrolled body for n={n} must emit exactly {n} select insns; got {select_count}"
        );
    }

    #[test]
    fn fx_hash_constants_match_relon_trace_abi() {
        // SEED / PRIME are private in `relon-trace-abi`, but their
        // observable behaviour is locked via this round-trip: feeding
        // an empty byte slice into `fx_hash_bytes` returns SEED;
        // feeding a single zero byte yields `(SEED ^ 0) * PRIME =
        // SEED * PRIME`.
        let seed_observed = relon_trace_abi::hash::fx_hash_bytes(b"");
        assert_eq!(
            seed_observed as i64, FX_HASH_SEED,
            "FX_HASH_SEED drift vs relon-trace-abi"
        );
        let seed_x_prime = relon_trace_abi::hash::fx_hash_bytes(&[0u8]);
        let expected = (FX_HASH_SEED as u64).wrapping_mul(FX_HASH_PRIME as u64);
        assert_eq!(
            seed_x_prime, expected,
            "FX_HASH_PRIME drift vs relon-trace-abi"
        );
    }
}
