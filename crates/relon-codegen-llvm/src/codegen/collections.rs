//! `Op`-family: record / collection construction.
//!
//! AllocRootRecord / StoreFieldAtRecord and their record-local backing.
//! ConstList* / ListGetByIntIdx / DictGetByStringKey are still in the
//! `unsupported` set in `super::lower_op` — see the per-op notes in
//! [`Emit::lower_collections_rest`]. Phase 0b fills the record-local
//! sub-record / tail-record arms here.

use inkwell::values::{IntValue, PointerValue};

use relon_ir::ir::{IrType, Op};

use crate::error::LlvmError;
use crate::state::ARENA_STATE_OFFSET_TAIL_CURSOR;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase 0b seam: list / dict / sub-record construction ops.
    /// Dispatched from `super::lower_op`. The record-local ops
    /// (`AllocSubRecord` / `PushRecordBase` /
    /// `EmitTailRecordFromAbsoluteAddr`) are ported from
    /// `relon-codegen-cranelift`'s `record.rs`. The `ConstList*` scalar
    /// arms resolve a const-pool offset (laid out by
    /// [`super::ConstPool`]) and push it as an `i32` — mirroring
    /// cranelift's `const_pool_emit::emit_const_value`. The remaining
    /// arms stay `unsupported`:
    ///
    /// * `ConstListString` is unsupported on the cranelift golden side
    ///   too (pointer-array materialisation is not wired anywhere yet).
    /// * `ListGetByIntIdx` / `DictGetByStringKey` are unsupported on
    ///   the cranelift golden side, so there is no semantic oracle to
    ///   port against.
    pub(crate) fn lower_collections_rest(
        &mut self,
        ip: usize,
        ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        match op {
            Op::ConstListInt { idx, .. } => self.emit_const_list(*idx, IrType::ListInt),
            Op::ConstListFloat { idx, .. } => self.emit_const_list(*idx, IrType::ListFloat),
            Op::ConstListBool { idx, .. } => self.emit_const_list(*idx, IrType::ListBool),
            Op::ConstListString { idx, .. } => self.emit_const_list(*idx, IrType::ListString),
            Op::ConstDict { idx, .. } => self.emit_const_list(*idx, IrType::Dict),
            Op::AllocSubRecord {
                record_local_idx,
                root_size,
                root_align,
            } => self.emit_alloc_sub_record(*record_local_idx, *root_size, *root_align),
            Op::PushRecordBase { record_local_idx } => {
                self.emit_push_record_base(*record_local_idx)
            }
            Op::EmitTailRecordFromAbsoluteAddr { ty } => {
                self.emit_tail_record_from_absolute(ip_hint, *ty)
            }
            _ => Err(LlvmError::Codegen(format!(
                "unsupported op (Phase 0b collections seam): {op:?} at ip={ip}"
            ))),
        }
    }

    /// Lower `Op::ConstListInt` / `ConstListFloat` / `ConstListBool` /
    /// `ConstListString`. For the scalar-element lists the pushed
    /// `ListInt/Float/Bool` value is the buffer-relative address of the
    /// `[len][payload]` record; for `ConstListString` it is the address
    /// of the pointer-array header (`[len][off_i...]`) a `keys[i]`
    /// consumer indexes. Resolves the record's byte offset (laid out by
    /// [`super::ConstPool`] when the module was scanned), materialises
    /// it as an `i32` const, and pushes it as the matching `List*`
    /// stack value — the buffer-relative address the host's arena-
    /// prefix copy resolves at runtime. Mirrors cranelift's
    /// `const_pool_emit::emit_const_value`.
    fn emit_const_list(&mut self, idx: u32, ty: IrType) -> Result<(), LlvmError> {
        let (offset, label) = match ty {
            IrType::ListInt => (self.const_pool.list_int_offsets.get(&idx), "ConstListInt"),
            IrType::ListFloat => (
                self.const_pool.list_float_offsets.get(&idx),
                "ConstListFloat",
            ),
            IrType::ListBool => (self.const_pool.list_bool_offsets.get(&idx), "ConstListBool"),
            IrType::ListString => (
                self.const_pool.list_string_offsets.get(&idx),
                "ConstListString",
            ),
            IrType::Dict => (self.const_pool.dict_offsets.get(&idx), "ConstDict"),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "emit_const_list: unexpected list type {other:?}"
                )));
            }
        };
        let off = offset.copied().ok_or_else(|| {
            LlvmError::Codegen(format!("{label} idx {idx} not in pre-computed const pool"))
        })?;
        let c = self.ctx.i32_type().const_int(u64::from(off), false);
        self.push(c, ty);
        Ok(())
    }

    /// Resolve / create the i32 alloca backing an
    /// `Op::AllocRootRecord` / `Op::AllocSubRecord` record-local
    /// index. Each variable holds an out_ptr-relative i32 offset.
    /// Mirrors cranelift's `get_or_create_record_local`.
    pub(crate) fn get_or_create_record_local(
        &mut self,
        idx: u32,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        if let Some(p) = self.record_locals.get(&idx).copied() {
            return Ok(p);
        }
        let i32_t = self.ctx.i32_type();
        let name = self.next_name("record_local");
        let slot = self
            .builder
            .build_alloca(i32_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("record_local alloca: {e}")))?;
        self.record_locals.insert(idx, slot);
        Ok(slot)
    }

    /// Lower `Op::AllocRootRecord { record_local_idx }`. The root
    /// record sits at `out_ptr + 0`; bind the record-local to constant
    /// `i32 0`. Subsequent `Op::StoreFieldAtRecord` ops uniformly
    /// compute `out_ptr + record_local + offset` from this slot.
    /// Mirrors cranelift's `emit_alloc_root_record`.
    pub(crate) fn emit_alloc_root_record(&mut self, idx: u32) -> Result<(), LlvmError> {
        // Phase D.2: fast-path entry has no arena to write into — the
        // matching `StoreFieldAtRecord` is rewritten to a store
        // against the `fast_ret_slot` alloca, which doesn't need a
        // record-local offset. Skip the alloca entirely so post-O3
        // IR stays free of the dead bookkeeping store.
        if self.fast_path.is_some() {
            let _ = idx;
            return Ok(());
        }
        let slot = self.get_or_create_record_local(idx)?;
        let zero = self.ctx.i32_type().const_zero();
        self.builder
            .build_store(slot, zero)
            .map_err(|e| LlvmError::Codegen(format!("AllocRootRecord store: {e}")))?;
        Ok(())
    }

    /// Read `state.tail_cursor`, align it up to `align`, bump it by
    /// `record_size`, store the new cursor back, and return the
    /// pre-bump aligned cursor (= the buffer-relative offset of the
    /// freshly-reserved tail record). Shared by the sub-record and
    /// tail-record arms; mirrors cranelift's `emit_tail_alloc` policy
    /// (bump-allocate inside the output buffer's tail region). Sets
    /// `needs_tail_cursor` so the buffer-protocol epilogue returns the
    /// post-bump cursor rather than the static `buffer_return_size`.
    ///
    /// `record_size` is an `i32` value (compile-time const for
    /// `AllocSubRecord`, runtime-computed for the tail-record copy).
    fn emit_tail_alloc(
        &mut self,
        record_size: IntValue<'ctx>,
        align: u32,
    ) -> Result<IntValue<'ctx>, LlvmError> {
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "tail alloc outside buffer-protocol entry shape (no state ptr)".into(),
            )
        })?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();
        let tail_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_TAIL_CURSOR), false)],
                    "tail_cursor_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("tail_cursor GEP: {e}")))?
        };
        let cur = self
            .builder
            .build_load(i32_t, tail_gep, "tail_cursor_pre")
            .map_err(|e| LlvmError::Codegen(format!("tail_cursor load: {e}")))?
            .into_int_value();
        let aligned = if align <= 1 {
            cur
        } else {
            let add = i32_t.const_int(u64::from(align - 1), false);
            let mask = i32_t.const_int(u64::from(!(align - 1)), false);
            let sum = self
                .builder
                .build_int_add(cur, add, "tail_align_sum")
                .map_err(|e| LlvmError::Codegen(format!("tail align add: {e}")))?;
            self.builder
                .build_and(sum, mask, "tail_align_and")
                .map_err(|e| LlvmError::Codegen(format!("tail align and: {e}")))?
        };
        let new_cur = self
            .builder
            .build_int_add(aligned, record_size, "tail_cursor_post")
            .map_err(|e| LlvmError::Codegen(format!("tail cur bump: {e}")))?;
        self.builder
            .build_store(tail_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("tail cursor store: {e}")))?;
        self.needs_tail_cursor = true;
        Ok(aligned)
    }

    /// Lower `Op::AllocSubRecord { record_local_idx, root_size,
    /// root_align }`. Bump-allocates `root_size` bytes in the output
    /// buffer's tail area (aligned up to `root_align`), binds the
    /// record-local to the pre-bump cursor (a buffer-relative offset),
    /// and bumps the cursor past the reserved record. Mirrors
    /// cranelift's `emit_alloc_sub_record`.
    pub(crate) fn emit_alloc_sub_record(
        &mut self,
        idx: u32,
        root_size: u32,
        root_align: u32,
    ) -> Result<(), LlvmError> {
        let size_v = self.ctx.i32_type().const_int(u64::from(root_size), false);
        let pre_cursor = self.emit_tail_alloc(size_v, root_align)?;
        let slot = self.get_or_create_record_local(idx)?;
        self.builder
            .build_store(slot, pre_cursor)
            .map_err(|e| LlvmError::Codegen(format!("AllocSubRecord store: {e}")))?;
        Ok(())
    }

    /// Lower `Op::PushRecordBase { record_local_idx }`. Reads the
    /// record-local and pushes its current value onto the operand
    /// stack so the surrounding parent record can store the
    /// sub-record's base offset into its pointer slot. Mirrors
    /// cranelift's `emit_push_record_base`.
    pub(crate) fn emit_push_record_base(&mut self, idx: u32) -> Result<(), LlvmError> {
        let slot = self.record_locals.get(&idx).copied().ok_or_else(|| {
            LlvmError::Codegen(format!(
                "PushRecordBase({idx}) before matching AllocRootRecord / AllocSubRecord"
            ))
        })?;
        let v = self
            .builder
            .build_load(self.ctx.i32_type(), slot, "record_base")
            .map_err(|e| LlvmError::Codegen(format!("PushRecordBase load: {e}")))?
            .into_int_value();
        self.push(v, IrType::I32);
        Ok(())
    }

    /// Lower `Op::EmitTailRecordFromAbsoluteAddr { ty }`. Pops an
    /// arena-relative source pointer (an `i32` offset where a
    /// `[len:u32 LE][payload]` record lives), copies that record into
    /// the output buffer's tail area at the aligned tail cursor, bumps
    /// the cursor past it, and pushes the pre-bump cursor (= the
    /// buffer-relative offset of the just-written record) onto the
    /// operand stack as an `i32`. The pushed value is what subsequent
    /// `Op::StoreFieldAtRecord { ty: String / ListInt / ... }` stores
    /// into a parent record's pointer slot. Mirrors cranelift's
    /// `emit_tail_record_from_absolute`.
    pub(crate) fn emit_tail_record_from_absolute(
        &mut self,
        ip_hint: &str,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        if matches!(ty, IrType::ListString) {
            // Pointer-array list: relocate inner offsets into the output
            // buffer's coordinate system via the shared block copy, then
            // push the copied header's buffer-relative offset for the
            // parent record's pointer slot.
            let header_off = self.pop_int(ip_hint)?;
            let new_header = self.copy_list_string_block(header_off)?;
            self.push(new_header, IrType::I32);
            return Ok(());
        }
        let src_off_i32 = self.pop_int(ip_hint)?;
        let i32_t = self.ctx.i32_type();
        // Read the record's leading `[len: u32]` header to size the
        // copy. `arena_addr_i32` resolves `arena_base + src_off`.
        let src_abs = self.arena_addr_i32(src_off_i32)?;
        let len_i32 = self
            .builder
            .build_load(i32_t, src_abs, "tail_rec_len")
            .map_err(|e| LlvmError::Codegen(format!("EmitTailRecord len load: {e}")))?
            .into_int_value();
        // record_size by type:
        //   String / ListBool : 4 + len  (len = byte / element count)
        //   ListInt / ListFloat : 8 + 8 * len  (8-byte header + i64/f64)
        let record_size = match ty {
            IrType::String | IrType::ListBool => {
                let four = i32_t.const_int(4, false);
                self.builder
                    .build_int_add(len_i32, four, "tail_rec_size4")
                    .map_err(|e| LlvmError::Codegen(format!("EmitTailRecord size4: {e}")))?
            }
            IrType::ListInt | IrType::ListFloat => {
                let three = i32_t.const_int(3, false);
                let shifted = self
                    .builder
                    .build_left_shift(len_i32, three, "tail_rec_shl")
                    .map_err(|e| LlvmError::Codegen(format!("EmitTailRecord shl: {e}")))?;
                let eight = i32_t.const_int(8, false);
                self.builder
                    .build_int_add(shifted, eight, "tail_rec_size8")
                    .map_err(|e| LlvmError::Codegen(format!("EmitTailRecord size8: {e}")))?
            }
            IrType::ListSchema | IrType::ListList => {
                return Err(LlvmError::Codegen(format!(
                    "EmitTailRecordFromAbsoluteAddr {ty:?} (pointer-array) not yet supported"
                )));
            }
            _ => {
                return Err(LlvmError::Codegen(format!(
                    "EmitTailRecordFromAbsoluteAddr unsupported {ty:?}"
                )));
            }
        };
        let align: u32 = match ty {
            IrType::String | IrType::ListBool => 4,
            IrType::ListInt | IrType::ListFloat => 8,
            _ => unreachable!("record_size match already rejected non-pointer-indirect types"),
        };
        let pre_cursor = self.emit_tail_alloc(record_size, align)?;
        // dest = arena_base + out_ptr + pre_cursor.
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr
        let dst_off = self
            .builder
            .build_int_add(out_ptr_i32, pre_cursor, "tail_rec_dst_off")
            .map_err(|e| LlvmError::Codegen(format!("EmitTailRecord dst off: {e}")))?;
        let dst_ptr = self.arena_addr_i32(dst_off)?;
        // The earlier `src_abs` GEP is still valid — both source and
        // destination pointers are pure address arithmetic off the
        // cached `arena_base_ptr`, no aliasing constraint between them.
        let i64_t = self.ctx.i64_type();
        let rec64 = self
            .builder
            .build_int_z_extend(record_size, i64_t, "tail_rec_size64")
            .map_err(|e| LlvmError::Codegen(format!("EmitTailRecord size zext: {e}")))?;
        self.builder
            .build_memcpy(dst_ptr, align, src_abs, 1, rec64)
            .map_err(|e| LlvmError::Codegen(format!("EmitTailRecord memcpy: {e}")))?;
        // Push the pre-bump cursor (buffer-relative offset of the
        // just-written record) as an i32. Mirrors cranelift's
        // post-copy `self.push(pre_cursor)`.
        self.push(pre_cursor, IrType::I32);
        Ok(())
    }

    /// Lower `Op::StoreFieldAtRecord { record_local_idx, offset, ty }`.
    /// Pops the top of the operand stack and writes it into
    /// `out_ptr + record_local + offset`. Mirrors cranelift's
    /// `emit_store_field_at_record` but without the explicit bounds
    /// check (LLVM AOT relies on the host's arena sizing).
    pub(crate) fn emit_store_field_at_record(
        &mut self,
        ip_hint: &str,
        idx: u32,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        // Phase D.2: fast-path entry rewrites the single-Int-field
        // record store into the `fast_ret_slot` store. Mirrors the
        // `Op::StoreField` rewrite — the profile gate guarantees the
        // return record carries exactly one Int field, so the matching
        // `StoreFieldAtRecord` at `profile.ret_offset` is the
        // function's actual return value. Any other shape (multi-
        // field record, branded sub-records) escapes the envelope
        // and surfaces as an emitter error.
        if let Some(fast) = self.fast_path.clone() {
            let _ = idx;
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreFieldAtRecord: only I64 returns supported, got {ty:?}"
                )));
            }
            if offset != fast.profile.ret_offset {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreFieldAtRecord: offset {offset} != profile.ret_offset {}",
                    fast.profile.ret_offset
                )));
            }
            let v = self.pop_int(ip_hint)?;
            self.builder.build_store(fast.ret_slot, v).map_err(|e| {
                LlvmError::Codegen(format!("fast StoreFieldAtRecord ret_slot: {e}"))
            })?;
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreFieldAtRecord outside buffer-protocol entry".into())
        })?;
        let value = self.pop_int(ip_hint)?;
        let slot = self.record_locals.get(&idx).copied().ok_or_else(|| {
            LlvmError::Codegen(format!(
                "StoreFieldAtRecord({idx}) before matching AllocRootRecord"
            ))
        })?;
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        // Read the record-local offset, add `offset`, add `out_ptr`,
        // then z-extend the sum into the i64 arena GEP index.
        let record_base = self
            .builder
            .build_load(i32_t, slot, "record_base")
            .map_err(|e| LlvmError::Codegen(format!("record_base load: {e}")))?
            .into_int_value();
        let off_const = i32_t.const_int(u64::from(offset), false);
        let slot_off = self
            .builder
            .build_int_add(record_base, off_const, "record_slot_off")
            .map_err(|e| LlvmError::Codegen(format!("record_slot_off: {e}")))?;
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr under buffer protocol
        let total_off = self
            .builder
            .build_int_add(out_ptr_i32, slot_off, "record_total_off")
            .map_err(|e| LlvmError::Codegen(format!("record_total_off: {e}")))?;
        let total_off64 = self
            .builder
            .build_int_z_extend(total_off, i64_t, "record_total_off_zext")
            .map_err(|e| LlvmError::Codegen(format!("record_total_off zext: {e}")))?;
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[total_off64], "record_dst")
                .map_err(|e| LlvmError::Codegen(format!("record_dst GEP: {e}")))?
        };
        // Emit the typed store. For `Bool` / `Null`, narrow the i32
        // stack slot to i8 before writing — matches the on-wire
        // record layout. For pointer-indirect types (`String`,
        // `List*`) the slot stores the i32 buffer-relative offset
        // verbatim.
        match ty {
            IrType::I64 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord I64: {e}")))?;
            }
            IrType::F64 => {
                // Stack carries f64 as bit-cast i64; restore the f64
                // payload for the store so the destination bytes
                // match the IEEE-754 wire layout.
                let f = self
                    .builder
                    .build_bit_cast(value, self.ctx.f64_type(), "record_f64_bitcast")
                    .map_err(|e| LlvmError::Codegen(format!("F64 bitcast: {e}")))?;
                self.builder
                    .build_store(addr, f)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord F64: {e}")))?;
            }
            IrType::I32
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::ListList
            | IrType::Closure
            | IrType::Dict => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord I32: {e}")))?;
            }
            IrType::Bool | IrType::Null => {
                let v8 = self
                    .builder
                    .build_int_truncate(value, i8_t, "record_bool_trunc")
                    .map_err(|e| {
                        LlvmError::Codegen(format!("StoreFieldAtRecord Bool trunc: {e}"))
                    })?;
                self.builder
                    .build_store(addr, v8)
                    .map_err(|e| LlvmError::Codegen(format!("StoreFieldAtRecord Bool: {e}")))?;
            }
        }
        Ok(())
    }
}
