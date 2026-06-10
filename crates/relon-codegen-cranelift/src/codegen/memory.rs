//! Memory primitives for [`super::Codegen`]: scratch allocation and
//! the family of `Op::Load*AtAbsolute` / `Op::Store*AtAbsolute` /
//! `Op::MemcpyAtAbsolute` arms.
//!
//! Every helper here works against the host arena (the contiguous
//! buffer the trampoline points `SandboxState.arena_base` at). The
//! helpers translate IR-level `i32` arena-relative offsets into
//! native pointers via [`super::Codegen::arena_addr`] and enforce
//! the bounds-check policy from [`crate::sandbox::SandboxConfig`].
//!
//! The helpers leave the operand stack discipline unchanged from the
//! wasm-side semantics: loads pop a base offset and push the loaded
//! value; stores pop `[base, value]` and emit no result.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{InstBuilder, MemFlags, Value as CValue};

use crate::error::CraneliftError;
use crate::sandbox::{
    TrapKind, STATE_OFFSET_ARENA_BASE, STATE_OFFSET_ARENA_LEN, STATE_OFFSET_SCRATCH_BASE,
    STATE_OFFSET_SCRATCH_CURSOR,
};

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Lower the inner step of `Op::AllocScratch` / `Op::AllocScratchDyn`:
    /// reserve `size` bytes in the scratch region of the arena and
    /// push the resulting arena-relative offset.
    ///
    /// Bumps `SandboxState.scratch_cursor` after the optional
    /// `scratch_base + cur + size <= arena_len` bounds check.
    pub(super) fn emit_alloc_scratch(&mut self, size: CValue) -> Result<(), CraneliftError> {
        let cur = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_SCRATCH_CURSOR,
        );
        let scratch_base = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_SCRATCH_BASE,
        );
        let arena_len = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_LEN,
        );
        // Bounds: scratch_base + cur + size <= arena_len.
        if self.sandbox.bounds_check {
            let base_plus_cur = self.builder.ins().iadd(scratch_base, cur);
            let end = self.builder.ins().iadd(base_plus_cur, size);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        // Push the arena-relative offset (scratch_base + pre_cursor).
        let off = self.builder.ins().iadd(scratch_base, cur);
        // Bump.
        let new_cur = self.builder.ins().iadd(cur, size);
        self.builder.ins().store(
            MemFlags::trusted(),
            new_cur,
            self.state_ptr,
            STATE_OFFSET_SCRATCH_CURSOR,
        );
        self.push(off);
        Ok(())
    }

    /// Lower `Op::AllocScratchDyn`. The size is popped from the
    /// virtual stack (must be an `i32`).
    pub(super) fn emit_alloc_scratch_dyn(&mut self) -> Result<(), CraneliftError> {
        let size = self.pop()?;
        self.emit_alloc_scratch(size)
    }

    /// Lower `Op::AllocScratch { size_bytes }`. The size is a
    /// compile-time constant.
    pub(super) fn emit_alloc_scratch_static(
        &mut self,
        size_bytes: u32,
    ) -> Result<(), CraneliftError> {
        let size = self.builder.ins().iconst(I32, i64::from(size_bytes));
        self.emit_alloc_scratch(size)
    }

    /// Translate an arena-relative `i32` offset (top of stack) to its
    /// absolute host address. Performs the standard `arena_base + off`
    /// computation plus an optional bounds check against `arena_len`.
    /// Pushes nothing — the caller decides what to do with the
    /// returned cranelift value.
    pub(super) fn arena_addr(
        &mut self,
        off_i32: CValue,
        slot_size: u32,
    ) -> Result<CValue, CraneliftError> {
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        if self.sandbox.bounds_check {
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let size = self.builder.ins().iconst(I32, i64::from(slot_size));
            let end = self.builder.ins().iadd(off_i32, size);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        let off_p = self.builder.ins().uextend(self.pointer_ty, off_i32);
        Ok(self.builder.ins().iadd(arena_base, off_p))
    }

    /// Lower `Op::LoadI32AtAbsolute { offset }`. Pops an arena-
    /// relative i32 base, adds `offset`, performs the bounds check
    /// (`base + offset + 4 <= arena_len`), loads 4 bytes, and pushes
    /// the resulting i32.
    pub(super) fn emit_load_i32_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 4)?;
        let v = self.builder.ins().load(I32, MemFlags::trusted(), abs, 0);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::LoadI64AtAbsolute { offset }`.
    pub(super) fn emit_load_i64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        let v = self.builder.ins().load(I64, MemFlags::trusted(), abs, 0);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::LoadF64AtAbsolute { offset }`.
    pub(super) fn emit_load_f64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        let v = self.builder.ins().load(
            cranelift_codegen::ir::types::F64,
            MemFlags::trusted(),
            abs,
            0,
        );
        self.push(v);
        Ok(())
    }

    /// Lower `Op::LoadI8UAtAbsolute { offset }`. Loads a single byte
    /// and zero-extends to i32.
    pub(super) fn emit_load_i8u_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 1)?;
        let b = self.builder.ins().load(
            cranelift_codegen::ir::types::I8,
            MemFlags::trusted(),
            abs,
            0,
        );
        let v = self.builder.ins().uextend(I32, b);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::StoreI32AtAbsolute { offset }`. Stack:
    /// `[base: i32, value: i32]`. Pops value first, then base.
    pub(super) fn emit_store_i32_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 4)?;
        self.builder.ins().store(MemFlags::trusted(), value, abs, 0);
        Ok(())
    }

    /// Lower `Op::StoreI64AtAbsolute { offset }`.
    pub(super) fn emit_store_i64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        self.builder.ins().store(MemFlags::trusted(), value, abs, 0);
        Ok(())
    }

    /// Lower `Op::StoreF64AtAbsolute { offset }`.
    pub(super) fn emit_store_f64_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 8)?;
        self.builder.ins().store(MemFlags::trusted(), value, abs, 0);
        Ok(())
    }

    /// Lower `Op::StoreI8AtAbsolute { offset }`. Pops i32 value;
    /// stores its low byte.
    pub(super) fn emit_store_i8_at_absolute(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let base = self.pop()?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base, off_v);
        let abs = self.arena_addr(composed, 1)?;
        let v8 = self
            .builder
            .ins()
            .ireduce(cranelift_codegen::ir::types::I8, value);
        self.builder.ins().store(MemFlags::trusted(), v8, abs, 0);
        Ok(())
    }

    /// Lower `Op::MemcpyAtAbsolute`. Stack: `[dest: i32, src: i32,
    /// len: i32]`. Translates each pointer through `arena_addr` and
    /// invokes libc memcpy via cranelift's `call_memcpy` helper.
    pub(super) fn emit_memcpy_at_absolute(&mut self) -> Result<(), CraneliftError> {
        let len = self.pop()?;
        let src_off = self.pop()?;
        let dest_off = self.pop()?;
        // Bounds-check both pointers using the len.
        if self.sandbox.bounds_check {
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let dest_end = self.builder.ins().iadd(dest_off, len);
            let cmp_d = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, dest_end, arena_len);
            self.cond_trap(cmp_d, TrapKind::BoundsViolation);
            let src_end = self.builder.ins().iadd(src_off, len);
            let cmp_s = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, src_end, arena_len);
            self.cond_trap(cmp_s, TrapKind::BoundsViolation);
        }
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let dest_p = self.builder.ins().uextend(self.pointer_ty, dest_off);
        let src_p = self.builder.ins().uextend(self.pointer_ty, src_off);
        let dest = self.builder.ins().iadd(arena_base, dest_p);
        let src = self.builder.ins().iadd(arena_base, src_p);
        let len_p = self.builder.ins().uextend(self.pointer_ty, len);
        self.builder
            .call_memcpy(self.frontend_config, dest, src, len_p);
        Ok(())
    }

    /// #165 — single-allocation N-operand string concat. Mirrors the
    /// shape of the bundled stdlib `concat` body
    /// ([`relon_ir::stdlib::defs::concat_string_string_body`]) but
    /// generalised: one scratch alloc sized `total_len + 4`, one
    /// header store, and N memcpys writing each operand payload at
    /// the running cursor. The N - 1 intermediate scratch records the
    /// unfolded `concat(concat(...), ...)` path used to emit are
    /// elided entirely — the win the [`Op::StrConcatN`] IR variant
    /// exists to deliver.
    ///
    /// Operand layout: each `String` IR value is an i32 arena offset
    /// pointing at a `[len: u32 LE][utf8 bytes]` record. The op pops
    /// `operand_count` such offsets from the operand stack (top-of-
    /// stack is the outer RHS, bottom is the deepest LHS leaf), then
    /// pushes one fresh i32 offset for the joined record.
    pub(super) fn emit_str_concat_n(&mut self, operand_count: u32) -> Result<(), CraneliftError> {
        if operand_count < 2 {
            return Err(CraneliftError::Codegen(format!(
                "Op::StrConcatN with operand_count={operand_count} (expected >= 2)"
            )));
        }
        let n = operand_count as usize;
        // Pop N i32 offsets, restore source order so the join reads
        // `s_0 || s_1 || ... || s_{n-1}` left-to-right.
        let mut offs: Vec<CValue> = Vec::with_capacity(n);
        for _ in 0..n {
            offs.push(self.pop()?);
        }
        offs.reverse();
        // Load the `[len: u32]` header for every operand once. Stored
        // in a parallel `lens` vector so we can both sum lengths and
        // drive the per-operand memcpy from the same i32 values
        // without re-loading.
        let mut lens: Vec<CValue> = Vec::with_capacity(n);
        for off in &offs {
            // Compute `addr = arena_addr(off, 4)` (4-byte header).
            let abs = self.arena_addr(*off, 4)?;
            let len = self.builder.ins().load(I32, MemFlags::trusted(), abs, 0);
            lens.push(len);
        }
        // total_len = sum of lens (i32 add fold).
        let mut total_len = lens[0];
        for v in &lens[1..] {
            total_len = self.builder.ins().iadd(total_len, *v);
        }
        // record_size = total_len + 4 (header)
        let four = self.builder.ins().iconst(I32, 4);
        let record_size = self.builder.ins().iadd(total_len, four);
        // Allocate the scratch record (one allocation for the entire
        // join — the perf win).
        self.emit_alloc_scratch(record_size)?;
        let base_off = self.pop()?;
        // Write header: i32.store(base, total_len)
        let base_abs = self.arena_addr(base_off, 4)?;
        self.builder
            .ins()
            .store(MemFlags::trusted(), total_len, base_abs, 0);
        // Walk operands in source order, copying each payload at the
        // running cursor.
        //   cursor_off = base_off + 4
        //   for each operand:
        //     memcpy(arena[cursor_off], arena[off + 4], len)
        //     cursor_off += len
        let mut cursor_off = self.builder.ins().iadd(base_off, four);
        for i in 0..n {
            let len = lens[i];
            let src_off_payload = self.builder.ins().iadd(offs[i], four);
            // Bounds-check both pointers once per copy when the
            // sandbox config asks for it (matches the existing
            // `Op::MemcpyAtAbsolute` policy).
            if self.sandbox.bounds_check {
                let arena_len = self.builder.ins().load(
                    I32,
                    MemFlags::trusted(),
                    self.state_ptr,
                    STATE_OFFSET_ARENA_LEN,
                );
                let dst_end = self.builder.ins().iadd(cursor_off, len);
                let cmp_d = self
                    .builder
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThan, dst_end, arena_len);
                self.cond_trap(cmp_d, TrapKind::BoundsViolation);
                let src_end = self.builder.ins().iadd(src_off_payload, len);
                let cmp_s = self
                    .builder
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThan, src_end, arena_len);
                self.cond_trap(cmp_s, TrapKind::BoundsViolation);
            }
            let arena_base = self.builder.ins().load(
                self.pointer_ty,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_BASE,
            );
            let dest_p = self.builder.ins().uextend(self.pointer_ty, cursor_off);
            let src_p = self.builder.ins().uextend(self.pointer_ty, src_off_payload);
            let dest = self.builder.ins().iadd(arena_base, dest_p);
            let src = self.builder.ins().iadd(arena_base, src_p);
            let len_p = self.builder.ins().uextend(self.pointer_ty, len);
            self.builder
                .call_memcpy(self.frontend_config, dest, src, len_p);
            cursor_off = self.builder.ins().iadd(cursor_off, len);
        }
        // Push the resulting record offset.
        self.push(base_off);
        Ok(())
    }

    /// Lower `Op::IntToStr` — pop one `I64`, materialise its base-10
    /// decimal `String` record in the scratch arena, push the record
    /// offset. Byte-exact with the tree-walker's `i64` `Display`
    /// rendering: a leading `-` for negatives, no leading zeros, `0`
    /// for zero, and `i64::MIN` → `-9223372036854775808`.
    ///
    /// Strategy (no libc itoa, so the wasm leg needs no import): work on
    /// the unsigned magnitude (`-i64::MIN` is computed via wrapping
    /// negation reinterpreted as `u64`, which is the correct magnitude),
    /// count the decimal digits in a first loop, then fill the record
    /// payload back-to-front in a second loop and prepend `-` when
    /// negative. The record is `[len: u32 LE][utf8 digits]`, the same
    /// layout `Op::StrConcatN` / `Op::ConstString` produce, so the
    /// result feeds straight into the f-string concat.
    pub(super) fn emit_int_to_str(&mut self) -> Result<(), CraneliftError> {
        let v = self.pop()?;
        let zero64 = self.builder.ins().iconst(I64, 0);
        let ten64 = self.builder.ins().iconst(I64, 10);
        let one32 = self.builder.ins().iconst(I32, 1);

        // is_neg = v < 0 (signed).
        let is_neg = self.builder.ins().icmp(IntCC::SignedLessThan, v, zero64);
        // mag = is_neg ? (0 - v) : v   — wrapping negate (correct for
        // i64::MIN); reinterpreted as an unsigned magnitude for the
        // unsigned div/rem below.
        let neg_v = self.builder.ins().isub(zero64, v);
        let mag = self.builder.ins().select(is_neg, neg_v, v);
        // sign_len = is_neg ? 1 : 0.
        let zero32 = self.builder.ins().iconst(I32, 0);
        let sign_len = self.builder.ins().select(is_neg, one32, zero32);

        // Pass 1: count decimal digits of `mag`.
        //   cnt = 1; t = mag; while t >= 10 { t /= 10; cnt += 1 }
        let count_hdr = self.builder.create_block();
        let count_body = self.builder.create_block();
        let count_done = self.builder.create_block();
        self.builder.append_block_param(count_hdr, I64); // t
        self.builder.append_block_param(count_hdr, I32); // cnt
        self.builder.append_block_param(count_done, I32); // final cnt
        self.builder
            .ins()
            .jump(count_hdr, &[mag.into(), one32.into()]);

        self.builder.switch_to_block(count_hdr);
        let t_val = self.builder.block_params(count_hdr)[0];
        let cnt_val = self.builder.block_params(count_hdr)[1];
        let cont = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, t_val, ten64);
        self.builder
            .ins()
            .brif(cont, count_body, &[], count_done, &[cnt_val.into()]);

        self.builder.switch_to_block(count_body);
        let t_next = self.builder.ins().udiv(t_val, ten64);
        let cnt_next = self.builder.ins().iadd(cnt_val, one32);
        self.builder
            .ins()
            .jump(count_hdr, &[t_next.into(), cnt_next.into()]);
        self.builder.seal_block(count_hdr);
        self.builder.seal_block(count_body);

        self.builder.switch_to_block(count_done);
        self.builder.seal_block(count_done);
        let digit_count = self.builder.block_params(count_done)[0];
        // total_len = digit_count + sign_len   (utf8 byte count).
        let total_len = self.builder.ins().iadd(digit_count, sign_len);
        // record_size = total_len + 4 (header), rounded up to a 4-byte
        // multiple. The scratch bump allocator hands out records back-
        // to-back, and the buffer-protocol return path aligns a String
        // payload up to 4 bytes when it copies the record into the out
        // buffer (`align_up(base + 4, 4)`). If this record left the
        // scratch cursor unaligned, the NEXT scratch record (e.g. the
        // `Op::StrConcatN` that joins an f-string's parts) would start
        // unaligned and the return-side alignment would skip its leading
        // bytes. Padding the allocation — header still stores the exact
        // `total_len` — keeps every record 4-aligned. The slack bytes
        // are never read (the header length bounds every consumer).
        let four = self.builder.ins().iconst(I32, 4);
        let raw_size = self.builder.ins().iadd(total_len, four);
        let three = self.builder.ins().iconst(I32, 3);
        let neg_four = self.builder.ins().iconst(I32, -4);
        let bumped = self.builder.ins().iadd(raw_size, three);
        let record_size = self.builder.ins().band(bumped, neg_four);

        // Allocate the record; pop its arena offset.
        self.emit_alloc_scratch(record_size)?;
        let base_off = self.pop()?;
        // Header: store total_len at base.
        let base_abs = self.arena_addr(base_off, 4)?;
        self.builder
            .ins()
            .store(MemFlags::trusted(), total_len, base_abs, 0);

        // Payload base offset = base_off + 4 + sign_len (digits start
        // after the optional sign). Write digits back-to-front.
        let payload_off = self.builder.ins().iadd(base_off, four);
        let digits_off = self.builder.ins().iadd(payload_off, sign_len);
        // cursor = digits_off + digit_count (one past the last digit).
        let end_off = self.builder.ins().iadd(digits_off, digit_count);

        // Pass 2: m = mag; cursor = end_off;
        //   do { d = m % 10; cursor -= 1; store('0'+d) at cursor; m /= 10 }
        //   while (m != 0)
        let write_hdr = self.builder.create_block();
        let write_done = self.builder.create_block();
        self.builder.append_block_param(write_hdr, I64); // m
        self.builder.append_block_param(write_hdr, I32); // cursor
        self.builder
            .ins()
            .jump(write_hdr, &[mag.into(), end_off.into()]);

        self.builder.switch_to_block(write_hdr);
        let m_val = self.builder.block_params(write_hdr)[0];
        let cur_val = self.builder.block_params(write_hdr)[1];
        let rem = self.builder.ins().urem(m_val, ten64);
        let rem32 = self.builder.ins().ireduce(I32, rem);
        let ascii0 = self.builder.ins().iconst(I32, b'0' as i64);
        let ch = self.builder.ins().iadd(rem32, ascii0);
        let cur_next = self.builder.ins().isub(cur_val, one32);
        let ch_abs = self.arena_addr(cur_next, 1)?;
        let ch8 = self
            .builder
            .ins()
            .ireduce(cranelift_codegen::ir::types::I8, ch);
        self.builder
            .ins()
            .store(MemFlags::trusted(), ch8, ch_abs, 0);
        let m_next = self.builder.ins().udiv(m_val, ten64);
        let more = self.builder.ins().icmp(IntCC::NotEqual, m_next, zero64);
        self.builder.ins().brif(
            more,
            write_hdr,
            &[m_next.into(), cur_next.into()],
            write_done,
            &[],
        );
        self.builder.seal_block(write_hdr);

        self.builder.switch_to_block(write_done);
        self.builder.seal_block(write_done);
        // Prepend '-' at payload_off when negative (digits_off ==
        // payload_off + 1 in that case, so the byte slot is free).
        let minus_done = self.builder.create_block();
        let minus_body = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_neg, minus_body, &[], minus_done, &[]);
        self.builder.switch_to_block(minus_body);
        let minus_abs = self.arena_addr(payload_off, 1)?;
        let minus_ch = self.builder.ins().iconst(I32, b'-' as i64);
        let minus8 = self
            .builder
            .ins()
            .ireduce(cranelift_codegen::ir::types::I8, minus_ch);
        self.builder
            .ins()
            .store(MemFlags::trusted(), minus8, minus_abs, 0);
        self.builder.ins().jump(minus_done, &[]);
        self.builder.seal_block(minus_body);
        self.builder.switch_to_block(minus_done);
        self.builder.seal_block(minus_done);

        self.push(base_off);
        Ok(())
    }

    /// Lower `Op::FloatToStr` — pop one `F64`, materialise its decimal
    /// `String` record in the scratch arena, push the record offset.
    ///
    /// Unlike [`Self::emit_int_to_str`] the rendering is not inlined:
    /// Rust's `f64` `Display` (shortest round-trip decimal) is far too
    /// large to transcribe as CLIF ops, and byte-exactness with the
    /// tree-walk oracle demands the *same* algorithm. The codegen
    /// therefore bitcasts the F64 value to its i64 bit pattern,
    /// pre-allocates a fixed-size scratch record
    /// (`FLOAT_TO_STR_RECORD_SIZE`, a 4-byte multiple so the scratch
    /// cursor stays record-aligned; the header stores the exact
    /// payload length), and calls the
    /// [`crate::vtable::VtableSlot::RelonF64ToStr`] host helper, which
    /// writes `[len: u32 LE][utf8 payload]` via the shared
    /// `relon_ir::float_str::format_f64_display` core. A negative
    /// return (defence-in-depth bounds refusal inside the helper)
    /// traps as a bounds violation — a half-written record is never
    /// observable.
    pub(super) fn emit_float_to_str(&mut self) -> Result<(), CraneliftError> {
        use relon_ir::float_str::FLOAT_TO_STR_RECORD_SIZE;

        let v = self.pop()?;
        // F64 value -> raw IEEE-754 bits in an i64 register (the
        // helper ABI carries the float as bits so no float-ABI
        // assumptions leak across the FFI edge).
        let bits = self.builder.ins().bitcast(I64, MemFlags::new(), v);

        // Fixed-size allocation: simpler than a digits-count pre-pass
        // (the payload length is only known after formatting) and
        // already 4-aligned. The header stores the exact length, so
        // the slack bytes are never read.
        self.emit_alloc_scratch_static(FLOAT_TO_STR_RECORD_SIZE)?;
        let base_off = self.pop()?;

        let inst = self.emit_host_fn_call(
            crate::vtable::VtableSlot::RelonF64ToStr,
            &[self.state_ptr, bits, base_off],
        );
        let written = self.builder.inst_results(inst)[0];
        let zero32 = self.builder.ins().iconst(I32, 0);
        let failed = self
            .builder
            .ins()
            .icmp(IntCC::SignedLessThan, written, zero32);
        self.cond_trap(failed, TrapKind::BoundsViolation);

        self.push(base_off);
        Ok(())
    }
}
