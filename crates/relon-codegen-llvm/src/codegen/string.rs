//! `Op`-family: string construction + search.
//!
//! StrConcatN, the `Add(String)` in-place-append / concat fast path, and
//! the `contains` const-needle / extern-shim lowerings (plus their
//! libc/host-shim declarations).

use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue,
};
use inkwell::module::Linkage;
use inkwell::{AddressSpace, IntPredicate};

use relon_ir::ir::IrType;

use crate::error::LlvmError;
use crate::state::{
    ARENA_STATE_OFFSET_SCRATCH_BASE, ARENA_STATE_OFFSET_SCRATCH_CURSOR,
};

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Lower `Op::StrConcatN { operand_count }`. Pops N i32 arena
    /// offsets, sums their `[len: u32]` headers, allocates one scratch
    /// record sized `total + 4`, stamps the header, then memcpys each
    /// operand's payload at the running cursor. Pushes the resulting
    /// i32 offset. Mirrors cranelift's `emit_str_concat_n`.
    pub(crate) fn emit_str_concat_n(&mut self, ip_hint: &str, operand_count: u32) -> Result<(), LlvmError> {
        if operand_count < 2 {
            return Err(LlvmError::Codegen(format!(
                "Op::StrConcatN with operand_count={operand_count} (expected >= 2)"
            )));
        }
        let n = operand_count as usize;
        let i32_t = self.ctx.i32_type();
        // Pop N i32 offsets; reverse so source-order matches stack-
        // order (deepest leaf is the first operand).
        let mut offs: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for _ in 0..n {
            offs.push(self.pop_int(ip_hint)?);
        }
        offs.reverse();
        // Load each operand's `[len: u32]` header once.
        let mut lens: Vec<IntValue<'ctx>> = Vec::with_capacity(n);
        for off in &offs {
            let addr = self.arena_addr_i32(*off)?;
            let name = self.next_name("strconcat_len");
            let l = self
                .builder
                .build_load(i32_t, addr, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN len load: {e}")))?
                .into_int_value();
            lens.push(l);
        }
        // total_len = Σ lens.
        let mut total_len = lens[0];
        for v in &lens[1..] {
            let name = self.next_name("strconcat_sumlen");
            total_len = self
                .builder
                .build_int_add(total_len, *v, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN sum: {e}")))?;
        }
        // record_size = total_len + 4 (header).
        let four = i32_t.const_int(4, false);
        let name = self.next_name("strconcat_recsize");
        let record_size = self
            .builder
            .build_int_add(total_len, four, &name)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN record_size: {e}")))?;
        // Allocate the scratch record.
        self.emit_alloc_scratch_common(record_size)?;
        let base_off = self.pop_int(ip_hint)?;
        // Write header: i32.store(base, total_len).
        let base_abs = self.arena_addr_i32(base_off)?;
        self.builder
            .build_store(base_abs, total_len)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN header store: {e}")))?;
        // Walk operands in source order, copying payloads at the
        // running cursor.
        let name = self.next_name("strconcat_cursor0");
        let mut cursor_off = self
            .builder
            .build_int_add(base_off, four, &name)
            .map_err(|e| LlvmError::Codegen(format!("StrConcatN cursor init: {e}")))?;
        for i in 0..n {
            let len = lens[i];
            let name = self.next_name("strconcat_srcoff");
            let src_off_payload = self
                .builder
                .build_int_add(offs[i], four, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN src off: {e}")))?;
            let dst_ptr = self.arena_addr_i32(cursor_off)?;
            let src_ptr = self.arena_addr_i32(src_off_payload)?;
            let i64_t = self.ctx.i64_type();
            let name = self.next_name("strconcat_lenzext");
            let len64 = self
                .builder
                .build_int_z_extend(len, i64_t, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN len zext: {e}")))?;
            self.builder
                .build_memcpy(dst_ptr, 1, src_ptr, 1, len64)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN memcpy: {e}")))?;
            let name = self.next_name("strconcat_cursornext");
            cursor_off = self
                .builder
                .build_int_add(cursor_off, len, &name)
                .map_err(|e| LlvmError::Codegen(format!("StrConcatN cursor bump: {e}")))?;
        }
        // Push the resulting record offset.
        self.push(base_off, IrType::String);
        Ok(())
    }

    /// Lower `Op::Add(IrType::String)` with the W3 reduce-accumulator
    /// fast path. Pops `[lhs_off, rhs_off]` (i32 arena offsets); emits a
    /// runtime branch that picks between:
    ///
    /// * **In-place append (fast)** — when `lhs` is the most recent
    ///   scratch allocation (`lhs_off + 4 + lhs_len == scratch_base +
    ///   scratch_cursor`), extend the existing record by `rhs_len`
    ///   bytes. Updates the header in-place, copies only the rhs
    ///   payload, bumps `scratch_cursor` by `rhs_len`. Result offset =
    ///   `lhs_off`. This is the W3 hot loop's steady-state path: every
    ///   iteration's freshly-built accumulator is the most recent
    ///   allocation, so concatenating one more byte costs O(1) (a
    ///   single byte store + cursor bump) instead of the historical
    ///   O(N) re-copy of the running accumulator.
    /// * **Full alloc + copy (slow)** — when the lhs sits somewhere
    ///   else in the arena (e.g. const-pool literal, scratch alloc
    ///   from a different sub-expression). Replicates the historical
    ///   `concat` stdlib body: allocate `lhs_len + rhs_len + 4` bytes
    ///   of scratch, stamp the header, memcpy both payloads. Result
    ///   offset = the freshly-allocated base.
    ///
    /// The two arms merge at a phi node, and the resulting i32 offset
    /// is pushed back tagged as [`IrType::String`].
    ///
    /// ## Correctness ground
    ///
    /// The in-place mutation overwrites both:
    /// * the existing `[len: u32]` header at `[lhs_off..lhs_off+4]`,
    /// * the bytes immediately past the existing payload, at
    ///   `[lhs_off+4+lhs_len .. lhs_off+4+lhs_len+rhs_len]`.
    ///
    /// The guard `lhs_off + 4 + lhs_len == scratch_base +
    /// scratch_cursor` ensures the bytes past the payload are inside
    /// the unallocated scratch tail — no other live data sits there.
    /// The result offset shares its base with the lhs, so any
    /// subsequent reader that previously held `lhs_off` would now see
    /// the longer record — but in the reduce pattern the lhs slot
    /// (`acc`) is immediately overwritten by the `LetSet` that follows
    /// `Op::Add(String)`, so no stale alias remains.
    ///
    /// The fast path also keeps `scratch_cursor` advanced by exactly
    /// the same byte count that the slow path would have advanced it
    /// for the fresh record (`rhs_len` extra bytes vs `lhs_len +
    /// rhs_len + 4` extra bytes for a full copy), so the arena's
    /// out-of-bounds budget is *strictly tighter* than the historical
    /// path — there is no new failure mode where the fast path
    /// exceeds the arena while the slow path would have fit.
    pub(crate) fn emit_str_add_inplace_or_concat(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::Add(String) outside buffer-protocol entry shape (no arena_base)".into(),
            )
        })?;
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "Op::Add(String) outside buffer-protocol entry shape (no state)".into(),
            )
        })?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();
        let i64_t = self.ctx.i64_type();

        // Pop in reverse order: stack is `[lhs, rhs]`, top is rhs.
        // Phase L W3: keep the TypedValue so we can read provenance
        // (notably `Provenance::ConstString { len, first_byte }`) to
        // pick the const-len fast path below. LLVM cannot prove the
        // const length on its own — the rhs offset is a runtime i32
        // that happens to point into the const-pool prefix, and the
        // `[len]` header at that offset is reloaded every iteration
        // because the in-place append's header store at `lhs_addr`
        // aliases against it from the optimiser's point of view.
        let rhs_tv = self.pop(ip_hint)?;
        let lhs_tv = self.pop(ip_hint)?;
        let rhs_off = rhs_tv.val;
        let lhs_off = lhs_tv.val;
        let rhs_const_len: Option<(u32, Option<u8>)> = match rhs_tv.prov {
            Provenance::ConstString { len, first_byte } => Some((len, first_byte)),
            _ => None,
        };
        // SAFETY: when the *lhs* is sourced from `Op::ConstString` the
        // operand points into the per-module const-pool prefix (read-
        // only). Allowing the in-place fast path to fire in that case
        // would write the new `[len]` header — and the appended payload
        // — *into the const pool*, corrupting every subsequent
        // `Op::ConstString` load. We deliberately do **not** propagate
        // const-len knowledge for the lhs: keep the runtime `[len]`
        // load + the `lhs_end == scratch_end` runtime guard. In
        // practice the const-pool record sits at a fixed prefix offset
        // and the scratch tail is past every literal, so the guard
        // mismatches and the slow path (fresh scratch alloc + double
        // memcpy) takes over for the W3 reduce's first iteration
        // (`acc = "" + "a"`). The const-len optimisation is restricted
        // to the rhs slot.
        let lhs_const_len: Option<u32> = None;
        // Bind to silence the unused-binding lint while keeping the
        // structural symmetry with `rhs_const_len`.
        let _ = lhs_tv;

        // Load lhs.len and rhs.len from header word at offset 0 of
        // each record. Phase L W3: when the operand is known
        // const-string (provenance carries the literal byte length),
        // skip the per-iter `[len]` header load and feed LLVM an
        // i32 const — this removes the alias hazard between the
        // in-place store at `lhs_addr` and the rhs header read.
        let lhs_addr = self.arena_addr_i32(lhs_off)?;
        let lhs_len = if let Some(len) = lhs_const_len {
            i32_t.const_int(u64::from(len), false)
        } else {
            self.builder
                .build_load(i32_t, lhs_addr, "stradd_lhs_len")
                .map_err(|e| LlvmError::Codegen(format!("Add(String) lhs len load: {e}")))?
                .into_int_value()
        };
        let rhs_len = if let Some((len, _)) = rhs_const_len {
            i32_t.const_int(u64::from(len), false)
        } else {
            let rhs_addr = self.arena_addr_i32(rhs_off)?;
            self.builder
                .build_load(i32_t, rhs_addr, "stradd_rhs_len")
                .map_err(|e| LlvmError::Codegen(format!("Add(String) rhs len load: {e}")))?
                .into_int_value()
        };

        // Read scratch_base + scratch_cursor from the arena state.
        let scratch_cur_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_CURSOR), false)],
                    "stradd_scratch_cur_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_cur GEP: {e}")))?
        };
        let scratch_cur = self
            .builder
            .build_load(i32_t, scratch_cur_gep, "stradd_scratch_cur")
            .map_err(|e| LlvmError::Codegen(format!("scratch_cur load: {e}")))?
            .into_int_value();
        let scratch_base_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_BASE), false)],
                    "stradd_scratch_base_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_base GEP: {e}")))?
        };
        let scratch_base = self
            .builder
            .build_load(i32_t, scratch_base_gep, "stradd_scratch_base")
            .map_err(|e| LlvmError::Codegen(format!("scratch_base load: {e}")))?
            .into_int_value();

        // lhs_end = lhs_off + 4 + lhs_len
        let four = i32_t.const_int(4, false);
        let lhs_off_plus_4 = self
            .builder
            .build_int_add(lhs_off, four, "stradd_lhs_off_plus4")
            .map_err(|e| LlvmError::Codegen(format!("stradd lhs+4: {e}")))?;
        let lhs_end = self
            .builder
            .build_int_add(lhs_off_plus_4, lhs_len, "stradd_lhs_end")
            .map_err(|e| LlvmError::Codegen(format!("stradd lhs_end: {e}")))?;
        // scratch_end = scratch_base + scratch_cursor
        let scratch_end = self
            .builder
            .build_int_add(scratch_base, scratch_cur, "stradd_scratch_end")
            .map_err(|e| LlvmError::Codegen(format!("stradd scratch_end: {e}")))?;
        let is_tail = self
            .builder
            .build_int_compare(IntPredicate::EQ, lhs_end, scratch_end, "stradd_is_tail")
            .map_err(|e| LlvmError::Codegen(format!("stradd cmp: {e}")))?;

        let fast_bb = self.ctx.append_basic_block(self.func, "stradd_fast");
        let slow_bb = self.ctx.append_basic_block(self.func, "stradd_slow");
        let merge_bb = self.ctx.append_basic_block(self.func, "stradd_merge");
        self.builder
            .build_conditional_branch(is_tail, fast_bb, slow_bb)
            .map_err(|e| LlvmError::Codegen(format!("stradd branch: {e}")))?;

        // --- fast path: in-place append ---
        self.builder.position_at_end(fast_bb);
        let total_len_fast = self
            .builder
            .build_int_add(lhs_len, rhs_len, "stradd_fast_total")
            .map_err(|e| LlvmError::Codegen(format!("stradd fast total: {e}")))?;
        // store updated header
        self.builder
            .build_store(lhs_addr, total_len_fast)
            .map_err(|e| LlvmError::Codegen(format!("stradd fast header store: {e}")))?;
        // Append the rhs payload onto the lhs tail. Phase L W3: when
        // the rhs is a known const string (the dominant W3 reduce
        // shape — `acc + "a"`), specialise the copy:
        //   * len == 1 — emit a single `store i8 byte, ptr` against
        //     the lhs tail; bypasses the memcpy intrinsic entirely
        //     so the LLVM mid-end sees just a one-byte store + cursor
        //     bump (matching `String::push_str("a")`).
        //   * len > 1 — still use `build_memcpy`, but pass an i64
        //     const for the size so LLVM's `expand-memcpy` lowering
        //     unrolls to inline loads/stores instead of an indirect
        //     `callq *memcpy`.
        //   * non-const — historical path: zext runtime rhs_len to
        //     i64 and hand it to memcpy.
        let fast_dst = self.arena_addr_i32(lhs_end)?;
        match rhs_const_len {
            Some((1, Some(byte))) => {
                let byte_const = i8_t.const_int(u64::from(byte), false);
                self.builder
                    .build_store(fast_dst, byte_const)
                    .map_err(|e| {
                        LlvmError::Codegen(format!("stradd fast inline-byte store: {e}"))
                    })?;
            }
            Some((len, _)) => {
                let rhs_payload_off = self
                    .builder
                    .build_int_add(rhs_off, four, "stradd_rhs_payload_off")
                    .map_err(|e| LlvmError::Codegen(format!("stradd rhs payload off: {e}")))?;
                let fast_src = self.arena_addr_i32(rhs_payload_off)?;
                let rhs_len64 = i64_t.const_int(u64::from(len), false);
                self.builder
                    .build_memcpy(fast_dst, 1, fast_src, 1, rhs_len64)
                    .map_err(|e| {
                        LlvmError::Codegen(format!("stradd fast memcpy (const-len): {e}"))
                    })?;
            }
            None => {
                let rhs_payload_off = self
                    .builder
                    .build_int_add(rhs_off, four, "stradd_rhs_payload_off")
                    .map_err(|e| LlvmError::Codegen(format!("stradd rhs payload off: {e}")))?;
                let fast_src = self.arena_addr_i32(rhs_payload_off)?;
                let rhs_len64 = self
                    .builder
                    .build_int_z_extend(rhs_len, i64_t, "stradd_rhs_len64")
                    .map_err(|e| LlvmError::Codegen(format!("stradd rhs_len zext: {e}")))?;
                self.builder
                    .build_memcpy(fast_dst, 1, fast_src, 1, rhs_len64)
                    .map_err(|e| LlvmError::Codegen(format!("stradd fast memcpy: {e}")))?;
            }
        }
        // bump scratch_cursor by rhs_len
        let new_cur = self
            .builder
            .build_int_add(scratch_cur, rhs_len, "stradd_fast_newcur")
            .map_err(|e| LlvmError::Codegen(format!("stradd fast new cur: {e}")))?;
        self.builder
            .build_store(scratch_cur_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("stradd fast cursor store: {e}")))?;
        let fast_end_bb = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| LlvmError::Codegen(format!("stradd fast->merge: {e}")))?;

        // --- slow path: full alloc + double memcpy ---
        self.builder.position_at_end(slow_bb);
        // total_len = lhs_len + rhs_len
        let total_len_slow = self
            .builder
            .build_int_add(lhs_len, rhs_len, "stradd_slow_total")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow total: {e}")))?;
        // record_size = total_len + 4
        let record_size = self
            .builder
            .build_int_add(total_len_slow, four, "stradd_slow_recsize")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow recsize: {e}")))?;
        self.emit_alloc_scratch_common(record_size)?;
        let base_off = self.pop_int(ip_hint)?;
        // write header at base
        let base_addr = self.arena_addr_i32(base_off)?;
        self.builder
            .build_store(base_addr, total_len_slow)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow header store: {e}")))?;
        // memcpy lhs payload to base+4
        let base_plus_4 = self
            .builder
            .build_int_add(base_off, four, "stradd_slow_basep4")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow base+4: {e}")))?;
        let dst1 = self.arena_addr_i32(base_plus_4)?;
        let lhs_payload_off = self
            .builder
            .build_int_add(lhs_off, four, "stradd_slow_lhsp")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow lhsp: {e}")))?;
        let src1 = self.arena_addr_i32(lhs_payload_off)?;
        // Phase L W3: hand LLVM an i64 const memcpy size whenever
        // the lhs / rhs comes from `Op::ConstString` so the
        // `expand-memcpy` lowering can unroll to inline stores
        // instead of an indirect `callq *memcpy`. Falls back to the
        // historical zext path for non-const operands.
        let lhs_len64: IntValue<'ctx> = if let Some(len) = lhs_const_len {
            i64_t.const_int(u64::from(len), false)
        } else {
            self.builder
                .build_int_z_extend(lhs_len, i64_t, "stradd_slow_lhs64")
                .map_err(|e| LlvmError::Codegen(format!("stradd slow lhs_len zext: {e}")))?
        };
        self.builder
            .build_memcpy(dst1, 1, src1, 1, lhs_len64)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow lhs memcpy: {e}")))?;
        // memcpy rhs payload to base+4+lhs_len
        let lhs_dst_cursor = self
            .builder
            .build_int_add(base_plus_4, lhs_len, "stradd_slow_cur2")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow cur2: {e}")))?;
        let dst2 = self.arena_addr_i32(lhs_dst_cursor)?;
        let rhs_payload_off2 = self
            .builder
            .build_int_add(rhs_off, four, "stradd_slow_rhsp")
            .map_err(|e| LlvmError::Codegen(format!("stradd slow rhsp: {e}")))?;
        let src2 = self.arena_addr_i32(rhs_payload_off2)?;
        let rhs_len64_slow: IntValue<'ctx> = if let Some((len, _)) = rhs_const_len {
            i64_t.const_int(u64::from(len), false)
        } else {
            self.builder
                .build_int_z_extend(rhs_len, i64_t, "stradd_slow_rhs64")
                .map_err(|e| LlvmError::Codegen(format!("stradd slow rhs_len zext: {e}")))?
        };
        self.builder
            .build_memcpy(dst2, 1, src2, 1, rhs_len64_slow)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow rhs memcpy: {e}")))?;
        let slow_end_bb = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| LlvmError::Codegen(format!("stradd slow->merge: {e}")))?;

        // --- merge: phi of lhs_off / base_off ---
        self.builder.position_at_end(merge_bb);
        let phi = self
            .builder
            .build_phi(i32_t, "stradd_result")
            .map_err(|e| LlvmError::Codegen(format!("stradd phi: {e}")))?;
        let lhs_off_val: BasicValueEnum<'ctx> = lhs_off.into();
        let base_off_val: BasicValueEnum<'ctx> = base_off.into();
        phi.add_incoming(&[(&lhs_off_val, fast_end_bb), (&base_off_val, slow_end_bb)]);
        let result = phi.as_basic_value().into_int_value();
        // arena_base_ptr is referenced implicitly inside arena_addr_i32;
        // bind it to silence the borrow checker.
        let _ = arena_base_ptr;
        self.push(result, IrType::String);
        Ok(())
    }

    /// Phase F.1: lower `contains(haystack: String, needle: String) ->
    /// Bool` by emitting a direct extern call to
    /// `relon_llvm_str_contains_arena` instead of inlining the bundled
    /// stdlib body. See the `str_helpers` module docs for the ABI and
    /// the rationale (W4 / W4_long gap vs LuaJIT closed by std's
    /// SIMD-backed `str::contains`).
    ///
    /// Operand stack contract: pops `needle_off` (top), then
    /// `haystack_off`. Pushes the i32 0/1 result tagged as
    /// [`IrType::Bool`] so downstream `If` / `BrIf` ops see the same
    /// width the inlined body would have produced.
    pub(crate) fn emit_str_contains_extern(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        // Pop in reverse order: IR pushes `[haystack, needle]`, so the
        // top-of-stack is the needle. We need to materialise the
        // pointers in declaration order (haystack first) for the call,
        // so collect the offsets first and resolve to pointers below.
        let needle_off = self.pop_int(ip_hint)?;
        let haystack_off = self.pop_int(ip_hint)?;
        self.emit_str_contains_extern_with_offsets(ip_hint, haystack_off, needle_off)
    }

    /// Phase H: shared "given already-popped i32 offsets, emit the
    /// extern shim call" backbone. Split out of
    /// [`Self::emit_str_contains_extern`] so the const-needle
    /// fast path can reuse the extern fallback for `needle.len() > 1`
    /// (where the inline byte-scan no longer wins over the shim's
    /// SIMD-backed Two-Way matcher).
    pub(crate) fn emit_str_contains_extern_with_offsets(
        &mut self,
        _ip_hint: &str,
        haystack_off: IntValue<'ctx>,
        needle_off: IntValue<'ctx>,
    ) -> Result<(), LlvmError> {
        // GEP into the cached arena base. Mirrors `emit_load_at_absolute`
        // / `emit_str_concat_n` — both produce `arena_base + off_i32`
        // pointers the inner ops then read through. The shim consumes
        // raw `*const u8` headers, so we hand the GEP result directly.
        let haystack_ptr = self.arena_addr_i32(haystack_off)?;
        let needle_ptr = self.arena_addr_i32(needle_off)?;

        // Declare (or look up) the extern shim. Idempotent so multiple
        // `contains` call sites in the same module share a single
        // declaration — LLVM's verifier rejects duplicate function
        // definitions but happily reuses an existing extern.
        let shim = self.declare_str_contains_extern();

        let call_name = self.next_name("str_contains_extern");
        let call_site = self
            .builder
            .build_call(
                shim,
                &[
                    BasicMetadataValueEnum::PointerValue(haystack_ptr),
                    BasicMetadataValueEnum::PointerValue(needle_ptr),
                ],
                &call_name,
            )
            .map_err(|e| LlvmError::Codegen(format!("str_contains call: {e}")))?;

        let ret_val = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v,
            inkwell::values::ValueKind::Instruction(_) => {
                return Err(LlvmError::Codegen(
                    "relon_llvm_str_contains_arena returned void; expected i32".into(),
                ));
            }
        };
        let ret_i32 = match ret_val {
            BasicValueEnum::IntValue(v) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "relon_llvm_str_contains_arena returned non-int {other:?}"
                )));
            }
        };
        // Bool is encoded as i32 (0 / 1) across the LLVM AOT envelope,
        // matching what the inlined `contains_string_body` would have
        // produced through `Op::Ne(I32)` against `0`. No truncation /
        // sign-extension needed — the shim returns the same 0/1 i32
        // shape downstream `BrIf` / `Eq(Bool)` consumers expect.
        self.push(ret_i32, IrType::Bool);
        Ok(())
    }

    /// Phase H: lower `contains(haystack, "literal") -> Bool` for the
    /// const-needle case detected at the `Op::Call` site.
    ///
    /// Operand stack contract: pops `needle_off` (top — discarded; we
    /// have the literal bytes), then `haystack_off`, pushes the i32
    /// 0/1 result as [`IrType::Bool`]. The needle's arena-record
    /// pointer is unused on the fast paths because we already know
    /// the bytes at compile time.
    ///
    /// Dispatch by needle length:
    /// - `0` — every haystack contains the empty string; push `i32(1)`
    ///   directly. Matches `core::str::contains("")`'s semantics and
    ///   the bundled stdlib body's `p_len == 0 → true` short-circuit.
    /// - `1` — emit an inline byte-scan loop against the cached
    ///   haystack record. LLVM 18's loop vectoriser recognises the
    ///   single-byte equality scan and lowers it to SSE2 `pcmpeqb` +
    ///   `pmovmskb` (the same SIMD memchr LuaJIT exploits via libc).
    ///   Skips the `relon_llvm_str_contains_arena` FFI boundary — no
    ///   IC atomic loads, no register save/restore, no spill of the
    ///   surrounding loop's IV / accumulator. Per-call cost drops
    ///   from ~5 ns (Phase G shim) to ~1.5-2 ns on x86_64. This is
    ///   the hot path for the W4 / W4_long cmp_lua rows (needle =
    ///   `"x"`).
    /// - `> 1` — fall through to the extern shim. The shim's
    ///   `compute_contains` uses `str::contains` with Rust's Two-Way
    ///   matcher; inlining that here would balloon the IR for no
    ///   measured win (the multi-byte case isn't on the W4 / W4_long
    ///   hot loop).
    pub(crate) fn emit_str_contains_const_needle(
        &mut self,
        ip_hint: &str,
        needle_bytes: &[u8],
    ) -> Result<(), LlvmError> {
        // Pop both operands up-front. For `len == 0` / `len == 1` we
        // discard `needle_off` — the inline path reads the needle byte
        // from the source-emitted `needle_bytes` slice. For `len > 1`
        // we forward both offsets to the shim path.
        let needle_off = self.pop_int(ip_hint)?;
        let haystack_off = self.pop_int(ip_hint)?;

        match needle_bytes.len() {
            0 => {
                // Empty needle: always matches. Push `i32(1)` typed as
                // Bool to match the inlined stdlib body's encoding.
                let one = self.ctx.i32_type().const_int(1, false);
                self.push(one, IrType::Bool);
                Ok(())
            }
            1 => self.emit_str_contains_inline_byte(ip_hint, haystack_off, needle_bytes[0]),
            _ => {
                // Multi-byte needle: shim with Two-Way matcher beats a
                // naive open-coded scan. Forward both offsets.
                self.emit_str_contains_extern_with_offsets(ip_hint, haystack_off, needle_off)
            }
        }
    }

    /// Phase H: emit a direct libc `memchr` call for the single-byte
    /// const-needle case. Pushes the i32 0/1 result tagged as
    /// [`IrType::Bool`].
    ///
    /// IR shape (haystack record at `arena_base + haystack_off` carries
    /// `[len_u32 LE][payload bytes]`):
    ///
    /// ```text
    /// hay_len   = load i32, ptr (arena_base + haystack_off)
    /// hay_payld = gep (arena_base + haystack_off + 4)
    /// hay_len64 = zext i32 hay_len -> i64
    /// res_ptr   = call ptr @memchr(ptr hay_payld, i32 needle_byte, i64 hay_len64)
    /// hit       = icmp ne ptr res_ptr, null
    /// result    = zext i1 hit -> i32
    /// ```
    ///
    /// ## Why direct libc memchr instead of an open-coded scan?
    ///
    /// LLVM 18's loop vectoriser refuses to vectorise the open-coded
    /// scan because the inner body has a data-dependent early exit
    /// (`if byte == needle break`). Without vectorisation the W4_long
    /// row's 256-byte haystack would walk byte-by-byte at ~1 ns / byte
    /// — a ~256 ns/iter regression vs the Phase G shim's SIMD-backed
    /// `core::slice::contains(&u8)` (which calls into the `memchr`
    /// crate's `memchr` function, in turn delegating to libc on
    /// Linux). Calling libc `memchr` directly gives us the same SIMD
    /// `pcmpeqb` + `pmovmskb` lowering glibc ships, *without* the
    /// Phase G shim's per-call IC + record-parsing overhead.
    ///
    /// ## Symbol resolution
    ///
    /// `memchr` is in libc, resolved by MCJIT's default `dlsym` lookup
    /// when the symbol is declared with [`Linkage::External`]. No
    /// explicit `engine.add_global_mapping` call is required (the
    /// Phase F.1 shim needed one because its symbol lives inside the
    /// relon-codegen-llvm dylib, which dlsym can't see from MCJIT).
    pub(crate) fn emit_str_contains_inline_byte(
        &mut self,
        _ip_hint: &str,
        haystack_off: IntValue<'ctx>,
        needle_byte: u8,
    ) -> Result<(), LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let four = i32_t.const_int(4, false);
        let needle_arg = i32_t.const_int(u64::from(needle_byte), false);

        // Materialise haystack record header + payload pointer.
        let hay_hdr_ptr = self.arena_addr_i32(haystack_off)?;
        let hay_len_name = self.next_name("strc_inl_haylen");
        let hay_len = self
            .builder
            .build_load(i32_t, hay_hdr_ptr, &hay_len_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline hay_len: {e}")))?
            .into_int_value();
        let payload_off_name = self.next_name("strc_inl_payoff");
        let payload_off = self
            .builder
            .build_int_add(haystack_off, four, &payload_off_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline payload_off: {e}")))?;
        let hay_payload_ptr = self.arena_addr_i32(payload_off)?;
        let hay_len64_name = self.next_name("strc_inl_haylen64");
        let hay_len64 = self
            .builder
            .build_int_z_extend(hay_len, i64_t, &hay_len64_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline hay_len64: {e}")))?;

        // Declare libc `memchr` once per module.
        let memchr_fn = self.declare_libc_memchr();
        let call_name = self.next_name("strc_inl_memchr");
        let call_site = self
            .builder
            .build_call(
                memchr_fn,
                &[
                    BasicMetadataValueEnum::PointerValue(hay_payload_ptr),
                    BasicMetadataValueEnum::IntValue(needle_arg),
                    BasicMetadataValueEnum::IntValue(hay_len64),
                ],
                &call_name,
            )
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline memchr call: {e}")))?;
        let res_ptr_basic = call_site.try_as_basic_value();
        let res_ptr = match res_ptr_basic {
            inkwell::values::ValueKind::Basic(BasicValueEnum::PointerValue(p)) => p,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "memchr returned non-pointer: {other:?}"
                )));
            }
        };
        let null_ptr = ptr_t.const_null();
        let hit_name = self.next_name("strc_inl_hit");
        let hit_i1 = self
            .builder
            .build_int_compare(IntPredicate::NE, res_ptr, null_ptr, &hit_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline cmp: {e}")))?;
        let res_name = self.next_name("strc_inl_res");
        let res_v = self
            .builder
            .build_int_z_extend(hit_i1, i32_t, &res_name)
            .map_err(|e| LlvmError::Codegen(format!("str_contains_inline zext: {e}")))?;
        self.push(res_v, IrType::Bool);
        Ok(())
    }

    /// Idempotent declaration of libc `memchr`. Returns the cached
    /// `FunctionValue` so callers can issue `build_call` without
    /// re-parsing the signature. MCJIT's default `dlsym` resolver
    /// picks up the libc symbol — no `engine.add_global_mapping` is
    /// required.
    pub(crate) fn declare_libc_memchr(&self) -> FunctionValue<'ctx> {
        const SYM: &str = "memchr";
        if let Some(f) = self.module.get_function(SYM) {
            return f;
        }
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        // memchr signature: const void *memchr(const void *s, int c, size_t n)
        let fn_ty = ptr_t.fn_type(&[ptr_t.into(), i32_t.into(), i64_t.into()], false);
        self.module
            .add_function(SYM, fn_ty, Some(Linkage::External))
    }

    /// Idempotent declaration of the
    /// [`crate::str_helpers::relon_llvm_str_contains_arena`] extern.
    /// Returns the cached `FunctionValue` so callers can issue
    /// `build_call` without re-parsing the signature on every call site.
    pub(crate) fn declare_str_contains_extern(&self) -> FunctionValue<'ctx> {
        let sym = crate::str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL;
        if let Some(f) = self.module.get_function(sym) {
            return f;
        }
        let i32_t = self.ctx.i32_type();
        let ptr_t = self.ctx.ptr_type(AddressSpace::default());
        let fn_ty = i32_t.fn_type(&[ptr_t.into(), ptr_t.into()], false);
        self.module
            .add_function(sym, fn_ty, Some(Linkage::External))
    }
}
