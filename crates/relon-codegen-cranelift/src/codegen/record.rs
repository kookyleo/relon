//! Record-construction lowering helpers for [`super::Codegen`].
//!
//! The dict-construction protocol uses three IR-level record-local
//! ops to thread per-record base offsets through the lowering pass:
//!
//! * `Op::AllocRootRecord` — bind a fresh record-local to `out_ptr +
//!   0` (the root sits at the start of the fixed area).
//! * `Op::AllocSubRecord` — bump-allocate `root_size` bytes in the
//!   tail area, align up to `root_align`, and bind the record-local
//!   to the pre-bump cursor (a buffer-relative offset).
//! * `Op::PushRecordBase` / `Op::StoreFieldAtRecord` /
//!   `Op::EmitTailRecordFromAbsoluteAddr` — read the record-local
//!   to compute the per-field destination, or copy a referenced
//!   record into the tail area and push its post-bump offset.
//!
//! Each helper owns the cranelift wiring for one op family. Calls
//! into [`super::Codegen::emit_tail_alloc`] (still in mod.rs alongside
//! the buffer-protocol bookkeeping) keep the bump-allocator policy in
//! one place; the helpers here just feed the right `size` / `align`
//! into it.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I8};
use cranelift_codegen::ir::{InstBuilder, MemFlags};
use cranelift_frontend::Variable;

use relon_ir::ir::IrType;

use crate::error::CraneliftError;
use crate::sandbox::{TrapKind, STATE_OFFSET_ARENA_BASE, STATE_OFFSET_ARENA_LEN};

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Lower `Op::EmitTailRecordFromAbsoluteAddr { ty }`. Pops an
    /// arena-relative source pointer (an `i32` offset where a
    /// `[len:u32 LE][payload]` record lives), memcpies the record
    /// into the output buffer's tail area at `tail_cursor`, bumps the
    /// cursor past the record, and pushes the **arena-absolute offset**
    /// (`out_ptr + pre-bump cursor`) of the just-written record onto the
    /// virtual stack as an `i32`. The pushed value is what subsequent
    /// `Op::StoreFieldAtRecord { ty: String / ListInt / ... }` stores
    /// into a parent record's pointer slot (F1 arena-absolute slot
    /// convention).
    pub(super) fn emit_tail_record_from_absolute(
        &mut self,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        if matches!(ty, IrType::ListString) {
            // Pointer-array list: the source block carries inner
            // arena-relative offsets that must be relocated into the
            // output buffer's coordinate system. Share the rigid-block
            // copy + relocation with the top-level `StoreField` path.
            let header_off = self.pop()?;
            let new_header = self.copy_list_string_block(header_off)?;
            self.push(new_header);
            return Ok(());
        }
        if matches!(ty, IrType::ListSchema | IrType::ListList) {
            return Err(CraneliftError::Codegen(format!(
                "EmitTailRecordFromAbsoluteAddr {ty:?} (pointer-array) not yet supported"
            )));
        }
        let src_off_i32 = self.pop()?;
        // Share the pointer-indirect record copy with
        // `emit_store_pointer_indirect` so the position-dependent inner
        // payload alignment is recomputed on each side (a verbatim
        // `memcpy` would drag the source's pad geometry and corrupt the
        // 8-aligned `List<Int>` / `List<Float>` payload when a list
        // *param* is returned by identity — its input-buffer record can
        // land at a different `% 8` residue than the output slot).
        let pre_cursor = self.emit_pointer_indirect_record_copy(src_off_i32, ty)?;
        // Push the pre-bump cursor.
        self.push(pre_cursor);
        Ok(())
    }

    /// Lower `Op::BuildVariantRecord`. The lowering pass has already
    /// selected the Option/Result tag and computed the record geometry.
    /// This helper only materialises those bytes in the output tail area
    /// and returns the arena-absolute record offset.
    pub(super) fn emit_build_variant_record(
        &mut self,
        tag: u8,
        record_size: u32,
        record_align: u32,
        payload_offset: Option<u32>,
        payload_ty: Option<IrType>,
    ) -> Result<(), CraneliftError> {
        let payload = match payload_ty {
            Some(_) => Some(self.pop()?),
            None => None,
        };
        if payload.is_some() != payload_offset.is_some() {
            return Err(CraneliftError::Codegen(
                "BuildVariantRecord payload metadata mismatch".into(),
            ));
        }

        let size_v = self.builder.ins().iconst(I32, i64::from(record_size));
        let base_rel = self.emit_tail_alloc(size_v, record_align)?;
        let out_ptr = self.get_local(2)?;
        let record_abs = self.builder.ins().iadd(out_ptr, base_rel);

        let tag_addr = self.arena_addr(record_abs, 1)?;
        let tag_v = self.builder.ins().iconst(I8, i64::from(tag));
        self.builder
            .ins()
            .store(MemFlags::trusted(), tag_v, tag_addr, 0);

        if let (Some(payload), Some(offset), Some(ty)) = (payload, payload_offset, payload_ty) {
            let off_v = self.builder.ins().iconst(I32, i64::from(offset));
            let slot_off = self.builder.ins().iadd(record_abs, off_v);
            let slot_size = match ty {
                IrType::I64 | IrType::F64 => 8,
                IrType::I32
                | IrType::String
                | IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
                | IrType::ListList
                | IrType::Closure
                | IrType::Dict => 4,
                IrType::Bool | IrType::Unit => 1,
            };
            let slot_addr = self.arena_addr(slot_off, slot_size)?;
            match ty {
                IrType::Bool | IrType::Unit => {
                    let v8 = self.builder.ins().ireduce(I8, payload);
                    self.builder
                        .ins()
                        .store(MemFlags::trusted(), v8, slot_addr, 0);
                }
                _ => {
                    self.builder
                        .ins()
                        .store(MemFlags::trusted(), payload, slot_addr, 0);
                }
            }
        }

        self.push(record_abs);
        Ok(())
    }

    /// Build a variant record in scratch. Used by closure bodies, which have
    /// `ArenaState` but no entry `out_ptr` local.
    pub(super) fn emit_build_variant_record_scratch(
        &mut self,
        tag: u8,
        record_size: u32,
        record_align: u32,
        payload_offset: Option<u32>,
        payload_ty: Option<IrType>,
    ) -> Result<(), CraneliftError> {
        let payload = match payload_ty {
            Some(_) => Some(self.pop()?),
            None => None,
        };
        if payload.is_some() != payload_offset.is_some() {
            return Err(CraneliftError::Codegen(
                "BuildVariantRecordScratch payload metadata mismatch".into(),
            ));
        }

        let alloc_size = record_size
            .checked_add(record_align.saturating_sub(1))
            .ok_or_else(|| {
                CraneliftError::Codegen("BuildVariantRecordScratch size overflow".into())
            })?;
        let size_v = self.builder.ins().iconst(I32, i64::from(alloc_size));
        self.emit_alloc_scratch(size_v)?;
        let raw = self.pop()?;
        let record_abs = if record_align <= 1 {
            raw
        } else {
            let add = self.builder.ins().iconst(I32, i64::from(record_align - 1));
            let mask = self
                .builder
                .ins()
                .iconst(I32, i64::from(!(record_align as i32 - 1)));
            let sum = self.builder.ins().iadd(raw, add);
            self.builder.ins().band(sum, mask)
        };

        let tag_addr = self.arena_addr(record_abs, 1)?;
        let tag_v = self.builder.ins().iconst(I8, i64::from(tag));
        self.builder
            .ins()
            .store(MemFlags::trusted(), tag_v, tag_addr, 0);

        if let (Some(payload), Some(offset), Some(ty)) = (payload, payload_offset, payload_ty) {
            let off_v = self.builder.ins().iconst(I32, i64::from(offset));
            let slot_off = self.builder.ins().iadd(record_abs, off_v);
            let slot_size = match ty {
                IrType::I64 | IrType::F64 => 8,
                IrType::I32
                | IrType::String
                | IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
                | IrType::ListList
                | IrType::Closure
                | IrType::Dict => 4,
                IrType::Bool | IrType::Unit => 1,
            };
            let slot_addr = self.arena_addr(slot_off, slot_size)?;
            match ty {
                IrType::Bool | IrType::Unit => {
                    let v8 = self.builder.ins().ireduce(I8, payload);
                    self.builder
                        .ins()
                        .store(MemFlags::trusted(), v8, slot_addr, 0);
                }
                _ => {
                    self.builder
                        .ins()
                        .store(MemFlags::trusted(), payload, slot_addr, 0);
                }
            }
        }

        self.push(record_abs);
        Ok(())
    }

    /// Build a pointer-array list header from `len` pointer values already
    /// on the operand stack. The resulting header lives in the output tail
    /// area and contains arena-absolute offsets, matching the host buffer
    /// reader's pointer-array convention.
    pub(super) fn emit_build_pointer_list(&mut self, len: u32) -> Result<(), CraneliftError> {
        let mut elems = Vec::with_capacity(len as usize);
        for _ in 0..len {
            elems.push(self.pop()?);
        }
        elems.reverse();

        let size = 4u32
            .checked_add(len.checked_mul(4).ok_or_else(|| {
                CraneliftError::Codegen("BuildPointerList length overflow".into())
            })?)
            .ok_or_else(|| CraneliftError::Codegen("BuildPointerList size overflow".into()))?;
        let size_v = self.builder.ins().iconst(I32, i64::from(size));
        let base_rel = self.emit_tail_alloc(size_v, 4)?;
        let out_ptr = self.get_local(2)?;
        let header_abs = self.builder.ins().iadd(out_ptr, base_rel);

        let header_addr = self.arena_addr(header_abs, 4)?;
        let len_v = self.builder.ins().iconst(I32, i64::from(len));
        self.builder
            .ins()
            .store(MemFlags::trusted(), len_v, header_addr, 0);

        for (idx, elem) in elems.into_iter().enumerate() {
            let off = 4 + (idx as i32) * 4;
            self.builder
                .ins()
                .store(MemFlags::trusted(), elem, header_addr, off);
        }

        self.push(header_abs);
        Ok(())
    }

    /// Resolve / create the cranelift variable that backs an
    /// `Op::AllocRootRecord` / `Op::AllocSubRecord` record-local
    /// index. Each variable holds an `i32` out_ptr-relative offset.
    pub(super) fn get_or_create_record_local(&mut self, idx: u32) -> Variable {
        if let Some(v) = self.record_locals.get(&idx).copied() {
            return v;
        }
        let v = self.builder.declare_var(I32);
        self.record_locals.insert(idx, v);
        v
    }

    /// Lower `Op::AllocRootRecord { record_local_idx }`. The root
    /// record sits at `out_ptr + 0` so we just bind the record-local
    /// to a constant `i32 0`. Subsequent `Op::StoreFieldAtRecord` /
    /// `Op::PushRecordBase` ops uniformly compute `out_ptr +
    /// record_local + offset`.
    pub(super) fn emit_alloc_root_record(&mut self, idx: u32) {
        let var = self.get_or_create_record_local(idx);
        let zero = self.builder.ins().iconst(I32, 0);
        self.builder.def_var(var, zero);
    }

    /// Lower `Op::AllocSubRecord { record_local_idx, root_size,
    /// root_align }`. Aligns `tail_cursor` up to `root_align`,
    /// bounds-checks against `arena_len - out_ptr`, stores the
    /// aligned cursor into the record-local, then bumps
    /// `tail_cursor` by `root_size`.
    pub(super) fn emit_alloc_sub_record(
        &mut self,
        idx: u32,
        root_size: u32,
        root_align: u32,
    ) -> Result<(), CraneliftError> {
        let size_v = self.builder.ins().iconst(I32, i64::from(root_size));
        let pre_cursor = self.emit_tail_alloc(size_v, root_align)?;
        let var = self.get_or_create_record_local(idx);
        self.builder.def_var(var, pre_cursor);
        Ok(())
    }

    /// Lower `Op::PushRecordBase { record_local_idx }`. Reads the
    /// record-local and pushes its current value onto the stack so
    /// the surrounding parent record can store the sub-record's
    /// base offset into its pointer slot.
    pub(super) fn emit_push_record_base(&mut self, idx: u32) -> Result<(), CraneliftError> {
        let var = *self.record_locals.get(&idx).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "PushRecordBase({idx}) before matching AllocRootRecord / AllocSubRecord"
            ))
        })?;
        let v = self.builder.use_var(var);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::AllocScratchRecord { .. }`. Bump-allocates inside the
    /// scratch region and stores the aligned arena-relative base in the
    /// record-local.
    pub(super) fn emit_alloc_scratch_record(
        &mut self,
        idx: u32,
        root_size: u32,
        root_align: u32,
    ) -> Result<(), CraneliftError> {
        let alloc_size = root_size
            .checked_add(root_align.saturating_sub(1))
            .ok_or_else(|| CraneliftError::Codegen("AllocScratchRecord size overflow".into()))?;
        let size_v = self.builder.ins().iconst(I32, i64::from(alloc_size));
        self.emit_alloc_scratch(size_v)?;
        let raw = self.pop()?;
        let aligned = if root_align <= 1 {
            raw
        } else {
            let add = self.builder.ins().iconst(I32, i64::from(root_align - 1));
            let mask = self
                .builder
                .ins()
                .iconst(I32, i64::from(!(root_align as i32 - 1)));
            let sum = self.builder.ins().iadd(raw, add);
            self.builder.ins().band(sum, mask)
        };
        let var = self.get_or_create_record_local(idx);
        self.builder.def_var(var, aligned);
        Ok(())
    }

    /// Lower `Op::PushRecordBaseAbsolute { .. }`. The record-local already
    /// stores an arena-relative base.
    pub(super) fn emit_push_record_base_absolute(
        &mut self,
        idx: u32,
    ) -> Result<(), CraneliftError> {
        let var = *self.record_locals.get(&idx).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "PushRecordBaseAbsolute({idx}) before matching AllocScratchRecord"
            ))
        })?;
        let v = self.builder.use_var(var);
        self.push(v);
        Ok(())
    }

    /// Lower `Op::StoreFieldAtRecordAbsolute { .. }`. Pops the top of the
    /// virtual stack and writes it into `record_local + offset`, where the
    /// record-local already contains an arena-relative scratch base.
    pub(super) fn emit_store_field_at_record_absolute(
        &mut self,
        idx: u32,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let var = *self.record_locals.get(&idx).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "StoreFieldAtRecordAbsolute({idx}) before matching AllocScratchRecord"
            ))
        })?;
        let record_base_i32 = self.builder.use_var(var);
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let slot_off_i32 = self.builder.ins().iadd(record_base_i32, off_v);
        let slot_size = match ty {
            IrType::I64 | IrType::F64 => 8,
            IrType::I32
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::ListList
            | IrType::Closure
            | IrType::Dict => 4,
            IrType::Bool | IrType::Unit => 1,
        };
        let dest = self.arena_addr(slot_off_i32, slot_size)?;
        match ty {
            IrType::I64 | IrType::F64 => {
                self.builder
                    .ins()
                    .store(MemFlags::trusted(), value, dest, 0);
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
                    .ins()
                    .store(MemFlags::trusted(), value, dest, 0);
            }
            IrType::Bool | IrType::Unit => {
                let v8 = self.builder.ins().ireduce(I8, value);
                self.builder.ins().store(MemFlags::trusted(), v8, dest, 0);
            }
        }
        Ok(())
    }

    /// Lower `Op::StoreFieldAtRecord { record_local_idx, offset, ty
    /// }`. Pops the top of the virtual stack and writes it into
    /// `out_ptr + record_local + offset`.
    pub(super) fn emit_store_field_at_record(
        &mut self,
        idx: u32,
        offset: u32,
        ty: IrType,
    ) -> Result<(), CraneliftError> {
        let value = self.pop()?;
        let var = *self.record_locals.get(&idx).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "StoreFieldAtRecord({idx}) before matching AllocRootRecord / AllocSubRecord"
            ))
        })?;
        let record_base_i32 = self.builder.use_var(var);
        // Compute absolute dest = arena_base + out_ptr + record_base
        // + offset. Bounds-check via the same arena_len comparison
        // `buffer_field_addr` uses, but parameterised by
        // `record_base + offset` instead of a fixed compile-time
        // offset.
        let arena_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            STATE_OFFSET_ARENA_BASE,
        );
        let out_ptr_i32 = self.get_local(2)?;
        let off_v = self.builder.ins().iconst(I32, i64::from(offset));
        let slot_off_i32 = self.builder.ins().iadd(record_base_i32, off_v);
        // Slot size for the bounds check: scalar -> {1, 4, 8};
        // pointer-indirect -> 4 (the slot stores an i32 offset).
        let slot_size = match ty {
            IrType::I64 | IrType::F64 => 8,
            IrType::I32
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool
            | IrType::ListString
            | IrType::ListSchema
            | IrType::ListList
            | IrType::Closure
            | IrType::Dict => 4,
            IrType::Bool | IrType::Unit => 1,
        };
        if self.sandbox.bounds_check {
            let arena_len = self.builder.ins().load(
                I32,
                MemFlags::trusted(),
                self.state_ptr,
                STATE_OFFSET_ARENA_LEN,
            );
            let size_v = self.builder.ins().iconst(I32, i64::from(slot_size));
            let off_total = self.builder.ins().iadd(out_ptr_i32, slot_off_i32);
            let end = self.builder.ins().iadd(off_total, size_v);
            let cmp = self
                .builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThan, end, arena_len);
            self.cond_trap(cmp, TrapKind::BoundsViolation);
        }
        // Build absolute pointer.
        let out_ptr = self.builder.ins().uextend(self.pointer_ty, out_ptr_i32);
        let slot_off_p = self.builder.ins().uextend(self.pointer_ty, slot_off_i32);
        let dest0 = self.builder.ins().iadd(arena_base, out_ptr);
        let dest = self.builder.ins().iadd(dest0, slot_off_p);
        // Emit the store. For `Bool` / `Unit`, the stack slot is i32
        // but the underlying record stores i8. For pointer-indirect
        // types the value is already an i32 buffer-relative offset.
        match ty {
            IrType::I64 | IrType::F64 => {
                self.builder
                    .ins()
                    .store(MemFlags::trusted(), value, dest, 0);
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
                    .ins()
                    .store(MemFlags::trusted(), value, dest, 0);
            }
            IrType::Bool | IrType::Unit => {
                let v8 = self
                    .builder
                    .ins()
                    .ireduce(cranelift_codegen::ir::types::I8, value);
                self.builder.ins().store(MemFlags::trusted(), v8, dest, 0);
            }
        }
        Ok(())
    }
}
