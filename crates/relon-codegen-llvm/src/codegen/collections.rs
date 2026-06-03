//! `Op`-family: record / collection construction.
//!
//! AllocRootRecord / StoreFieldAtRecord and their record-local backing.
//! ConstList* / ListGetByIntIdx / DictGetByStringKey / AllocSubRecord /
//! PushRecordBase / EmitTailRecordFromAbsoluteAddr are still in the
//! `unsupported` set in `super::lower_op` — Phase 0b fills them here.

use inkwell::values::PointerValue;

use relon_ir::ir::IrType;

use crate::error::LlvmError;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Resolve / create the i32 alloca backing an
    /// `Op::AllocRootRecord` / `Op::AllocSubRecord` record-local
    /// index. Each variable holds an out_ptr-relative i32 offset.
    /// Mirrors cranelift's `get_or_create_record_local`.
    pub(crate) fn get_or_create_record_local(&mut self, idx: u32) -> Result<PointerValue<'ctx>, LlvmError> {
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
            | IrType::Closure => {
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
