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

/// Soft cap on `entry_count` for the inline form. Above this the
/// emitter should fall back to the helper call so the per-trace
/// machine-code footprint stays bounded. (The IR shape is constant
/// in `entry_count` — the loop is data-driven — so this is purely a
/// guard against arbitrarily-large dicts.) The cap is generous: W5
/// has 10 entries, and even a 64-entry table compiles to a single
/// tight loop with one load + one cmp + one brif per entry.
pub const MAX_INLINE_ENTRY_HINT: u32 = 64;

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
    let seed_i64 = builder.ins().iconst(I64, FX_HASH_SEED);
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
    let prime_v = builder.ins().iconst(I64, FX_HASH_PRIME);
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
    // verified by DictShapeGuard).
    let entry_count_u32 = builder.ins().load(I32, MemFlags::trusted(), dict_ptr, 8);
    let entry_count = builder.ins().uextend(I64, entry_count_u32);
    let entries_base = builder.ins().iadd_imm(dict_ptr, 12);
    builder.seal_block(scan_init);

    // Scan header: carries the current entry index.
    let scan_header = builder.create_block();
    builder.append_block_param(scan_header, I64); // scan_idx
    let scan_seed = builder.ins().iconst(I64, 0);
    builder
        .ins()
        .jump(scan_header, &[BlockArg::Value(scan_seed)]);
    builder.switch_to_block(scan_header);
    let scan_idx = builder.block_params(scan_header)[0];

    let exhausted = builder.ins().icmp(IntCC::Equal, scan_idx, entry_count);
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

    // Each entry is 16 bytes: [u64 hash][i64 value]. Compute the
    // entry's address from `scan_idx`. cranelift folds the shift
    // through its register allocator; this is a single `lea` on
    // x86_64.
    let sixteen = builder.ins().iconst(I64, 16);
    let entry_off = builder.ins().imul(scan_idx, sixteen);
    let entry_addr = builder.ins().iadd(entries_base, entry_off);
    let entry_hash = builder.ins().load(I64, MemFlags::trusted(), entry_addr, 0);
    let is_hit = builder.ins().icmp(IntCC::Equal, entry_hash, final_hash);

    let hit_block = builder.create_block();
    let scan_next = builder.create_block();
    builder.ins().brif(is_hit, hit_block, &[], scan_next, &[]);
    builder.seal_block(hit_block);
    builder.seal_block(scan_next);

    // ----- Scan next ----------------------------------------------------
    builder.switch_to_block(scan_next);
    let one = builder.ins().iconst(I64, 1);
    let next_scan = builder.ins().iadd(scan_idx, one);
    builder
        .ins()
        .jump(scan_header, &[BlockArg::Value(next_scan)]);
    builder.seal_block(scan_header);

    // ----- Hit block ----------------------------------------------------
    builder.switch_to_block(hit_block);
    let value = builder.ins().load(I64, MemFlags::trusted(), entry_addr, 8);

    // ----- Join ---------------------------------------------------------
    let join_block = builder.create_block();
    builder.append_block_param(join_block, I64);
    builder.ins().jump(join_block, &[BlockArg::Value(value)]);
    builder.switch_to_block(join_block);
    builder.seal_block(join_block);
    builder.block_params(join_block)[0]
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
