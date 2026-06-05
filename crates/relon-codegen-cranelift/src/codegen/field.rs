//! Buffer-protocol field load / store + tail-cursor allocator
//! helpers for [`super::Codegen`].
//!
//! Buffer-protocol entries see the input record at `in_ptr` (wasm
//! slot 0) and write into the output buffer rooted at `out_ptr`
//! (slot 2). Fixed-area scalar slots resolve through
//! [`Codegen::buffer_field_addr`]; pointer-indirect records (String /
//! ListInt / ListFloat / ListBool) ride the tail-cursor protocol
//! managed by [`Codegen::emit_tail_alloc`] and
//! [`Codegen::emit_store_pointer_indirect`].
//!
//! [`Codegen::emit_read_string_len`] sits in this file too because
//! its bounds-check + length-prefix decode mirrors the same arena-
//! relative pointer arithmetic the other helpers use.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{InstBuilder, MemFlags, Value as CValue};

use relon_ir::ir::IrType;

use crate::error::CraneliftError;
use crate::sandbox::{
    TrapKind, STATE_OFFSET_ARENA_BASE, STATE_OFFSET_ARENA_LEN, STATE_OFFSET_TAIL_CURSOR,
};

use super::{field_load_shape, pointer_indirect_record_align, EntryShape};

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Buffer-protocol mode: compute the absolute host address for a
    /// `(buf_local_idx, byte_offset, slot_size)` triple, after a
    /// bounds check against `state.arena_len`. Returns the absolute
    /// pointer-typed cranelift value, suitable for direct
    /// `load`/`store` with `MemFlags::trusted()` and zero immediate
    /// offset.
    ///
    /// `buf_local_idx` is the IR's wasm-local slot — 0 for `in_ptr`,
    /// 2 for `out_ptr` — read through `get_local`. `slot_size` is
    /// the byte width the caller is about to touch; the bounds check
    /// verifies `buf_ptr + byte_offset + slot_size <= arena_len`.
    pub(super) fn buffer_field_addr(
        &mut self,
        buf_local_idx: u32,
        byte_offset: u32,
        slot_size: u32,
    ) -> Result<CValue, CraneliftError> {
        // buf_ptr is i32 (the wasm handshake slot).
        let buf_ptr_i32 = self.get_local(buf_local_idx)?;
        // Widen to pointer-sized arithmetic so we never lose bits on
        // 64-bit hosts. `uextend` because the wasm-side semantics
        // treat the i32 as an unsigned byte offset.
        let buf_ptr = self.builder.ins().uextend(self.pointer_ty, buf_ptr_i32);

        // arena_base: load pointer-sized field from state.
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let arena_len_i32 = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_LEN,
        );

        // Bounds: required_end = byte_offset + slot_size; trap when
        // (buf_ptr + required_end) > arena_len. Doing the add as i32
        // mirrors the wasm-side semantics where the in/out pointer
        // is itself an i32 offset.
        if self.sandbox.bounds_check {
            let required_end = byte_offset
                .checked_add(slot_size)
                .ok_or_else(|| CraneliftError::Codegen("buffer field offset overflow".into()))?;
            let req_v = self.builder.ins().iconst(I32, i64::from(required_end));
            let end_i32 = self.builder.ins().iadd(buf_ptr_i32, req_v);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end_i32, arena_len_i32);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }

        // Compute absolute address = arena_base + buf_ptr + offset.
        let abs0 = self.builder.ins().iadd(arena_base, buf_ptr);
        let off_v = self
            .builder
            .ins()
            .iconst(self.pointer_ty, i64::from(byte_offset));
        let abs = self.builder.ins().iadd(abs0, off_v);
        Ok(abs)
    }

    /// Lower `Op::LoadField { offset, ty }`. Reads from
    /// `in_ptr + offset` (wasm slot 0) and pushes the value onto the
    /// virtual stack.
    ///
    /// In lambda mode (Stage 5 Phase C.4 closure body), the base
    /// pointer is the captures struct base (`captures_ptr` block-
    /// param) rather than `in_ptr` — this matches the wasm-side
    /// closure ABI which reuses `LoadField` for "read this captured
    /// value at this offset".
    pub(super) fn emit_load_field(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        let (cr_ty, size, push_ty) = field_load_shape(ty)?;
        let addr = if let Some(captures_ptr) = self.mode.captures_ptr() {
            // Lambda mode: arena_base + captures_ptr + offset.
            let off_v = self.builder.ins().iconst(I32, i64::from(offset));
            let composed = self.builder.ins().iadd(captures_ptr, off_v);
            self.arena_addr(composed, size)?
        } else {
            if !matches!(self.entry_shape, EntryShape::BufferProtocol) {
                return Err(CraneliftError::Codegen(
                    "LoadField outside buffer-protocol entry shape".into(),
                ));
            }
            self.buffer_field_addr(0 /* in_ptr */, offset, size)?
        };
        let loaded = self.builder.ins().load(cr_ty, MemFlags::trusted(), addr, 0);
        // For `Bool` / `Null` the IR's virtual stack expects an i32
        // slot — widen the loaded byte to i32 zero-extended.
        let val = match ty {
            IrType::Bool | IrType::Null => self.builder.ins().uextend(I32, loaded),
            _ => loaded,
        };
        let _ = push_ty;
        self.push(val);
        Ok(())
    }

    /// Lower `Op::StoreField { offset, ty }`. Pops the top of the
    /// virtual stack and writes it into `out_ptr + offset` (wasm slot
    /// 2). Scalar (I64 / F64 / I32 / Bool / Null) stores go through a
    /// direct cranelift `store`. Pointer-indirect stores (String /
    /// ListInt / ListFloat / ListBool) route through
    /// [`Codegen::emit_store_pointer_indirect`], which mirrors the
    /// wasm-side tail-cursor protocol: pop the source pointer, memcpy
    /// the `[len:4][payload]` record into `out_ptr + tail_cursor`,
    /// store `tail_cursor` (the new buffer-relative offset) into the
    /// fixed-area slot, and bump `tail_cursor`. ListString routes to
    /// [`Codegen::emit_store_list_string`] (the pointer-array variant
    /// with per-entry offset relocation); ListSchema stays unsupported.
    pub(super) fn emit_store_field(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        if !matches!(self.entry_shape, EntryShape::BufferProtocol) {
            return Err(CraneliftError::Codegen(
                "StoreField outside buffer-protocol entry shape".into(),
            ));
        }
        if matches!(
            ty,
            IrType::String | IrType::ListInt | IrType::ListFloat | IrType::ListBool
        ) {
            return self.emit_store_pointer_indirect(offset, ty);
        }
        if matches!(ty, IrType::ListString) {
            return self.emit_store_list_string(offset);
        }
        if matches!(ty, IrType::ListSchema) {
            return Err(CraneliftError::Codegen(format!(
                "StoreField pointer-indirect type {ty:?} (pointer-array) not yet supported",
            )));
        }
        let (cr_ty, size, _push_ty) = field_load_shape(ty)?;
        let value = self.pop()?;
        // For `Bool` / `Null` the stack slot is i32 but the store
        // width is i8.
        let store_val = match ty {
            IrType::Bool | IrType::Null => self
                .builder
                .ins()
                .ireduce(cranelift_codegen::ir::types::I8, value),
            _ => value,
        };
        let store_ty = match ty {
            IrType::Bool | IrType::Null => cranelift_codegen::ir::types::I8,
            _ => cr_ty,
        };
        let addr = self.buffer_field_addr(2 /* out_ptr */, offset, size)?;
        let _ = store_ty; // cranelift `store` infers width from value type
        self.builder
            .ins()
            .store(MemFlags::trusted(), store_val, addr, 0);
        Ok(())
    }

    /// Bump-allocate `size` bytes inside the output buffer's tail
    /// region.
    ///
    /// Mirrors the wasm-side `emit_tail_alloc` helper:
    ///
    /// 1. Align `state.tail_cursor` up to `align` (must be a power of
    ///    two — typical values are 4 / 8).
    /// 2. Bounds-check `aligned_cursor + size <= arena_len -
    ///    out_ptr`. We fold `out_ptr` into the comparison by
    ///    comparing `out_ptr + aligned_cursor + size` against
    ///    `arena_len`.
    /// 3. Record the new cursor in `state.tail_cursor`.
    /// 4. Return the **pre-bump** cursor — the slot the caller will
    ///    write into. The caller adds `out_ptr` (and optionally
    ///    `arena_base`) to materialise an absolute address.
    ///
    /// Returns the pre-bump cursor as an `i32` cranelift value (i.e.
    /// the buffer-relative offset of the freshly reserved region).
    /// The bump cursor is written back to `state.tail_cursor` so the
    /// next `emit_tail_alloc` (or the trampoline reading
    /// `tail_cursor()`) sees the post-bump value.
    pub(super) fn emit_tail_alloc(
        &mut self,
        size: CValue,
        align: u32,
    ) -> Result<CValue, CraneliftError> {
        // Read current cursor.
        let cur = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_TAIL_CURSOR,
        );
        // Align up to `align`. `align <= 1` keeps the cursor as-is.
        let aligned = if align <= 1 {
            cur
        } else {
            let add = self.builder.ins().iconst(I32, i64::from(align as i32 - 1));
            let mask = self
                .builder
                .ins()
                .iconst(I32, i64::from(!(align as i32 - 1)));
            let sum = self.builder.ins().iadd(cur, add);
            self.builder.ins().band(sum, mask)
        };
        // Bounds-check: out_ptr + aligned + size <= arena_len.
        // The out_ptr we use is the wasm-side handshake slot (local
        // 2), holding the absolute offset into the arena where the
        // out_buf starts.
        if self.sandbox.bounds_check {
            let out_ptr = self.get_local(2)?;
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let end0 = self.builder.ins().iadd(out_ptr, aligned);
            let end = self.builder.ins().iadd(end0, size);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        // Bump and persist the new cursor.
        let new_cur = self.builder.ins().iadd(aligned, size);
        self.builder.ins().store(
            MemFlags::trusted(),
            new_cur,
            self.state_ptr,
            STATE_OFFSET_TAIL_CURSOR,
        );
        Ok(aligned)
    }

    /// Lower `Op::StoreField { ty }` for a pointer-indirect type
    /// (`String` / `ListInt` / `ListFloat` / `ListBool`). Pops the
    /// source pointer (an arena-relative i32 offset where a
    /// `[len:u32 LE][payload]` record lives), memcpies the record into
    /// `out_ptr + tail_cursor`, writes `tail_cursor` (the buffer-
    /// relative offset of the just-written record) into the fixed-
    /// area slot at `offset`, and bumps `tail_cursor`.
    pub(super) fn emit_store_pointer_indirect(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        let src_off_i32 = self.pop()?;
        // Compute record_size from the in-record length prefix.
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let src_off_p = self.builder.ins().uextend(self.pointer_ty, src_off_i32);
        let src_abs = self.builder.ins().iadd(arena_base, src_off_p);
        // Load element / byte count from src+0.
        let len_i32 = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), src_abs, 0);
        let record_size = match ty {
            IrType::String => {
                // record_size = payload_len + 4
                let four = self.builder.ins().iconst(I32, 4);
                self.builder.ins().iadd(len_i32, four)
            }
            IrType::ListInt | IrType::ListFloat => {
                // record_size = 8 + 8 * element_count
                let three = self.builder.ins().iconst(I32, 3);
                let shifted = self.builder.ins().ishl(len_i32, three);
                let eight = self.builder.ins().iconst(I32, 8);
                self.builder.ins().iadd(shifted, eight)
            }
            IrType::ListBool => {
                // record_size = 4 + element_count
                let four = self.builder.ins().iconst(I32, 4);
                self.builder.ins().iadd(len_i32, four)
            }
            _ => {
                return Err(CraneliftError::Codegen(format!(
                    "emit_store_pointer_indirect: unsupported {ty:?}"
                )));
            }
        };
        let align = pointer_indirect_record_align(ty)?;
        // Reserve the tail slot.
        let pre_cursor = self.emit_tail_alloc(record_size, align)?;
        // Compute absolute dest = arena_base + out_ptr + pre_cursor.
        let out_ptr_i32 = self.get_local(2)?;
        let out_ptr = self.builder.ins().uextend(self.pointer_ty, out_ptr_i32);
        let pre_cursor_p = self.builder.ins().uextend(self.pointer_ty, pre_cursor);
        let dest0 = self.builder.ins().iadd(arena_base, out_ptr);
        let dest = self.builder.ins().iadd(dest0, pre_cursor_p);
        // memcpy(dest, src_abs, record_size).
        let size_p = self.builder.ins().uextend(self.pointer_ty, record_size);
        self.builder
            .call_memcpy(self.frontend_config, dest, src_abs, size_p);
        // Store pre_cursor (the buffer-relative offset) at the fixed-
        // area slot `out_ptr + offset`.
        let slot_addr = self.buffer_field_addr(2 /* out_ptr */, offset, 4)?;
        self.builder
            .ins()
            .store(MemFlags::trusted(), pre_cursor, slot_addr, 0);
        Ok(())
    }

    /// Lower `Op::StoreField { ty: ListString }` — the pointer-*array*
    /// marshalling. Mirrors the LLVM backend's
    /// `emit_store_field_list_string`.
    ///
    /// The source record (materialised by `Op::ConstListString`) is one
    /// contiguous arena block:
    ///
    /// ```text
    ///   [str_0 record][str_1]...[str_{N-1}][header]
    /// ```
    ///
    /// with each `str_i` a 4-aligned `[slen: u32][utf8]` record and the
    /// header a 4-aligned `[len: u32][off_0: u32]...[off_{N-1}: u32]`
    /// whose `off_i` are *arena-relative* offsets to `str_i`. The String
    /// records sit before the header, so `off_0` is the block's lowest
    /// offset and the block spans `[off_0, header_end)`.
    ///
    /// The whole block moves rigidly into the output buffer's tail, so a
    /// single `delta = dst_block_bufrel - src_block_start_arena` (a
    /// multiple of 4, preserving inner alignment) relocates every inner
    /// pointer: `new_off_i = off_i + delta`, `new_header = header_off +
    /// delta`. We memcpy the block, stamp `new_header` into the fixed
    /// slot, then walk the copied header's offset array adding `delta` to
    /// each entry — rewriting arena coordinates into the out-buffer ones
    /// `BufferReader::read_list_string` walks.
    pub(super) fn emit_store_list_string(&mut self, offset: u32) -> Result<(), CraneliftError> {
        let header_off = self.pop()?;
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );

        // len = [header_off].
        let header_off_p = self.builder.ins().uextend(self.pointer_ty, header_off);
        let header_abs = self.builder.ins().iadd(arena_base, header_off_p);
        let len = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), header_abs, 0);

        // offsets_end = header_off + 4 + len*4.
        let four = self.builder.ins().iconst(I32, 4);
        let two = self.builder.ins().iconst(I32, 2);
        let offs_bytes = self.builder.ins().ishl(len, two);
        let header_payload = self.builder.ins().iadd(header_off, four);
        let offsets_end = self.builder.ins().iadd(header_payload, offs_bytes);

        // src_block_start = (len != 0) ? off_0 : header_off, where
        // off_0 = [header_off + 4]. Empty list → block is just the header.
        let off0 = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), header_abs, 4);
        let zero = self.builder.ins().iconst(I32, 0);
        let len_nz = self.builder.ins().icmp(IntCC::NotEqual, len, zero);
        let src_block_start = self.builder.ins().select(len_nz, off0, header_off);
        let block_size = self.builder.ins().isub(offsets_end, src_block_start);

        // Reserve the tail slot (4-aligned) and compute dst.
        let dst_block = self.emit_tail_alloc(block_size, 4)?;
        let out_ptr_i32 = self.get_local(2)?;
        let out_ptr = self.builder.ins().uextend(self.pointer_ty, out_ptr_i32);
        let dst_block_p = self.builder.ins().uextend(self.pointer_ty, dst_block);
        let src_block_p = self.builder.ins().uextend(self.pointer_ty, src_block_start);
        let dest0 = self.builder.ins().iadd(arena_base, out_ptr);
        let dest = self.builder.ins().iadd(dest0, dst_block_p);
        let src_abs = self.builder.ins().iadd(arena_base, src_block_p);
        let size_p = self.builder.ins().uextend(self.pointer_ty, block_size);
        self.builder
            .call_memcpy(self.frontend_config, dest, src_abs, size_p);

        // delta = dst_block - src_block_start (multiple of 4).
        let delta = self.builder.ins().isub(dst_block, src_block_start);

        // new_header_bufrel = header_off + delta -> fixed-area slot.
        let new_header = self.builder.ins().iadd(header_off, delta);
        let slot_addr = self.buffer_field_addr(2 /* out_ptr */, offset, 4)?;
        self.builder
            .ins()
            .store(MemFlags::trusted(), new_header, slot_addr, 0);

        // entries_base (buffer-relative) = new_header + 4; absolute base
        // = arena_base + out_ptr + entries_base.
        let entries_base = self.builder.ins().iadd(new_header, four);
        let entries_base_p = self.builder.ins().uextend(self.pointer_ty, entries_base);
        let entries_abs = self.builder.ins().iadd(dest0, entries_base_p);

        // Relocation loop: for i in 0..len, *(entries_abs + i*4) += delta.
        let header_blk = self.builder.create_block();
        let body_blk = self.builder.create_block();
        let done_blk = self.builder.create_block();
        self.builder.append_block_param(header_blk, I32);
        let i0 = self.builder.ins().iconst(I32, 0);
        self.builder.ins().jump(header_blk, &[i0.into()]);

        self.builder.switch_to_block(header_blk);
        let i_val = self.builder.block_params(header_blk)[0];
        let cond = self.builder.ins().icmp(IntCC::UnsignedLessThan, i_val, len);
        self.builder.ins().brif(cond, body_blk, &[], done_blk, &[]);

        self.builder.switch_to_block(body_blk);
        let i_bytes = self.builder.ins().ishl(i_val, two);
        let i_bytes_p = self.builder.ins().uextend(self.pointer_ty, i_bytes);
        let entry_addr = self.builder.ins().iadd(entries_abs, i_bytes_p);
        let old = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), entry_addr, 0);
        let new = self.builder.ins().iadd(old, delta);
        self.builder
            .ins()
            .store(MemFlags::trusted(), new, entry_addr, 0);
        let one = self.builder.ins().iconst(I32, 1);
        let i_next = self.builder.ins().iadd(i_val, one);
        self.builder.ins().jump(header_blk, &[i_next.into()]);

        self.builder.switch_to_block(done_blk);
        self.builder.seal_block(header_blk);
        self.builder.seal_block(body_blk);
        self.builder.seal_block(done_blk);
        Ok(())
    }

    /// Lower `Op::LoadSchemaPtr { offset }`.
    ///
    /// A schema-typed `#main` parameter arrives in the input buffer as
    /// a 4-byte buffer-relative offset stored at `in_ptr + offset`.
    /// This op lifts that slot to the schema instance's buffer-relative
    /// base address: it reads the 4-byte slot, then adds `in_ptr` so a
    /// downstream `LoadFieldAtAbsolute` (which composes `arena_base +
    /// base + field_offset`) lands on the matching field. Mirrors the
    /// LLVM backend's `emit_load_schema_ptr`.
    ///
    /// The pushed value is the buffer-relative i32 base; the IR-level
    /// schema brand is tracked by the lowering pass, not by an
    /// operand-stack tag.
    pub(super) fn emit_load_schema_ptr(&mut self, offset: u32) -> Result<(), CraneliftError> {
        if !matches!(self.entry_shape, EntryShape::BufferProtocol) {
            return Err(CraneliftError::Codegen(
                "LoadSchemaPtr outside buffer-protocol entry shape".into(),
            ));
        }
        // Read the 4-byte buffer-relative offset at `in_ptr + offset`.
        let slot_addr = self.buffer_field_addr(0 /* in_ptr */, offset, 4)?;
        let rel_off = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), slot_addr, 0);
        // Lift to the schema instance's buffer-relative base
        // (`in_ptr + rel_off`). LoadFieldAtAbsolute adds `arena_base`.
        let in_ptr = self.get_local(0)?;
        let abs = self.builder.ins().iadd(in_ptr, rel_off);
        self.push(abs);
        Ok(())
    }

    /// Lower `Op::LoadFieldAtAbsolute { offset, ty }`. Stack:
    /// `[i32 base] -> [T]`. Pops a buffer-relative base address (pushed
    /// by `LoadSchemaPtr`), composes `arena_base + base + offset`,
    /// loads a value of `ty`, and pushes it. Mirrors
    /// [`Codegen::emit_load_field`] but the base pointer comes off the
    /// stack rather than from `in_ptr`. Scalar leaves only:
    /// pointer-indirect field types (`String` / `List*` / nested
    /// `Schema`) surface as `Unsupported` since their materialisation
    /// (`LoadStringPtr` etc.) is not yet lowered on the cranelift side.
    pub(super) fn emit_load_field_at_absolute(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        match ty {
            IrType::I64 | IrType::F64 | IrType::I32 | IrType::Bool | IrType::Null => {}
            other => {
                return Err(CraneliftError::Codegen(format!(
                    "LoadFieldAtAbsolute: pointer-indirect field type {other:?} not yet \
                     materialised on the cranelift backend (scalar schema fields only)"
                )));
            }
        }
        let (cr_ty, size, _push_ty) = field_load_shape(ty)?;
        let base_i32 = self.pop()?;
        // composed buffer-relative offset = base + field_offset.
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let composed = self.builder.ins().iadd(base_i32, off_v);
        let addr = self.arena_addr(composed, size)?;
        let loaded = self.builder.ins().load(cr_ty, MemFlags::trusted(), addr, 0);
        let val = match ty {
            IrType::Bool | IrType::Null => self.builder.ins().uextend(I32, loaded),
            _ => loaded,
        };
        self.push(val);
        Ok(())
    }

    /// Lower `Op::ReadStringLen`. Pops an i32 arena-relative pointer
    /// (a String or List* record's base), loads the leading 4-byte
    /// length prefix, and pushes it widened to i64. The bounds check
    /// verifies the 4 bytes fit inside the arena.
    pub(super) fn emit_read_string_len(&mut self) -> Result<(), CraneliftError> {
        let ptr_i32 = self.pop()?;
        // Widen ptr to host pointer width.
        let ptr = self.builder.ins().uextend(self.pointer_ty, ptr_i32);
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let arena_len_i32 = self.builder.ins().load(
            I32,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_LEN,
        );
        // Bounds: ptr + 4 <= arena_len.
        if self.sandbox.bounds_check {
            let four = self.builder.ins().iconst(I32, 4);
            let end_i32 = self.builder.ins().iadd(ptr_i32, four);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end_i32, arena_len_i32);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        let abs = self.builder.ins().iadd(arena_base, ptr);
        let len_i32 = self.builder.ins().load(I32, MemFlags::trusted(), abs, 0);
        let len_i64 = self.builder.ins().uextend(I64, len_i32);
        self.push(len_i64);
        Ok(())
    }
}
