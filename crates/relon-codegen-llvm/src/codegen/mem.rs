//! `Op`-family: memory / buffer-protocol I/O + arena addressing.
//!
//! LoadField/StoreField (scalar buffer slots), the pointer-indirect
//! param loads, raw `Load*/Store*AtAbsolute`, memcpy, scratch alloc, and
//! the arena-relative address composition helpers. `LoadFieldAtAbsolute`
//! (the dynamic-base schema-field load) is lowered here too via
//! `lower_mem_rest` (Phase 0b).

use inkwell::values::{BasicValueEnum, IntValue, PointerValue};

use relon_ir::ir::{IrType, Op};

use crate::error::LlvmError;
use crate::state::{
    ARENA_STATE_OFFSET_SCRATCH_BASE, ARENA_STATE_OFFSET_SCRATCH_CURSOR,
    ARENA_STATE_OFFSET_TAIL_CURSOR,
};

use super::*;

/// Variants of the absolute-pointer load lowering paths.
#[derive(Clone, Copy)]
pub(crate) enum AbsLoad {
    I32,
    I64,
    I8U,
    F64,
}

/// Variants of the absolute-pointer store lowering paths.
#[derive(Clone, Copy)]
pub(crate) enum AbsStore {
    I32,
    I64,
    I8,
    F64,
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase 0b seam: absolute-addressed field load
    /// (`LoadFieldAtAbsolute`). Dispatched from `super::lower_op`.
    ///
    /// Semantics (see `relon_ir::ir::Op::LoadFieldAtAbsolute`): pop an
    /// i32 arena-relative address pointing at the first byte of a
    /// schema instance's fixed area, then push the field at `offset`
    /// of type `ty`. This is the dynamic-base sibling of
    /// [`Self::emit_load_field`] — instead of implicitly reading the
    /// `in_ptr` handshake slot, the base address rides the operand
    /// stack. The address composition (`arena_base + addr + offset`)
    /// reuses [`Self::compose_abs_addr`], which keeps the i32-arena
    /// offset width-agnostic (zext to i64 then GEP from the i8*
    /// `arena_base`), so no 64-bit pointer width is baked in — see the
    /// `TODO(P3-wasm32)` note this file already carries.
    pub(crate) fn lower_mem_rest(
        &mut self,
        ip: usize,
        ip_hint: &str,
        op: &Op,
    ) -> Result<(), LlvmError> {
        match op {
            Op::LoadFieldAtAbsolute { offset, ty } => {
                self.emit_load_field_at_absolute(ip_hint, *offset, *ty)
            }
            _ => Err(LlvmError::Codegen(format!(
                "unsupported op (Phase 0b mem seam): {op:?} at ip={ip}"
            ))),
        }
    }

    /// Lower `Op::LoadFieldAtAbsolute { offset, ty }`. Stack:
    /// `[i32 addr] -> [T]`. Pops the arena-relative base address,
    /// composes `arena_base + addr + offset`, loads `ty`, and pushes
    /// the result on the int-typed virtual stack. The per-type load /
    /// widen logic mirrors [`Self::emit_load_field`]: F64 loads a
    /// `double` then bit-casts to i64 bits so the operand stack stays
    /// integer-typed, and Bool / Null (i8 on the wire) zero-extend to
    /// i32 to match the IR's virtual-stack convention.
    ///
    /// No bounds check — same "trust the IR + LLVM trap on UB" stance
    /// the rest of the `*AtAbsolute` family takes (Phase 3 wires the
    /// trap-propagation work).
    pub(crate) fn emit_load_field_at_absolute(
        &mut self,
        ip_hint: &str,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        let base = self.pop_int(ip_hint)?;
        let addr = self.compose_abs_addr(base, offset)?;
        if ty == IrType::F64 {
            let name = self.next_name("loadfa_f64");
            let f = self
                .builder
                .build_load(self.ctx.f64_type(), addr, &name)
                .map_err(|e| LlvmError::Codegen(format!("LoadFieldAtAbsolute f64 load: {e}")))?;
            let bits = self
                .builder
                .build_bit_cast(f, self.ctx.i64_type(), &self.next_name("loadfa_f64_bits"))
                .map_err(|e| LlvmError::Codegen(format!("LoadFieldAtAbsolute f64 bitcast: {e}")))?
                .into_int_value();
            self.push(bits, IrType::F64);
            return Ok(());
        }
        let (llvm_ty, push_ty) = self.field_load_kind(ty)?;
        let name = self.next_name("loadfa");
        let raw = self
            .builder
            .build_load(llvm_ty, addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("LoadFieldAtAbsolute load: {e}")))?
            .into_int_value();
        let widened = match push_ty {
            IrType::Bool | IrType::Null => {
                let name = self.next_name("loadfa_zext");
                self.builder
                    .build_int_z_extend(raw, self.ctx.i32_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadFieldAtAbsolute zext: {e}")))?
            }
            _ => raw,
        };
        self.push(widened, push_ty);
        Ok(())
    }

    /// Emit a LoadField — buffer-protocol only. The LLVM IR loads
    /// `arena_base + in_ptr + offset` for a value of `ty`. Phase D.1
    /// fast-path mode short-circuits this into a direct LLVM param
    /// access against the matching arg slot.
    pub(crate) fn emit_load_field(&mut self, offset: u32, ty: IrType) -> Result<(), LlvmError> {
        // Phase D.1 fast path: lift the buffer-protocol field load
        // into a direct LLVM param read whenever the field's offset
        // matches one of the profile's declared arg offsets.
        if let Some(fast) = self.fast_path.as_ref() {
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path LoadField: only I64 args supported, got {ty:?}"
                )));
            }
            let slot = fast
                .profile
                .arg_offsets
                .iter()
                .position(|&o| o == offset)
                .ok_or_else(|| {
                    LlvmError::Codegen(format!(
                        "fast-path LoadField: offset {offset} not in profile.arg_offsets"
                    ))
                })?;
            // LLVM param `slot` is the i64 arg directly under the
            // fast-entry signature (no implicit state slot, no
            // handshake i32 quartet).
            let p = self.func.get_nth_param(slot as u32).ok_or_else(|| {
                LlvmError::Codegen(format!(
                    "fast-path LoadField: llvm param #{slot} missing on function"
                ))
            })?;
            let v = p.into_int_value();
            self.push(v, IrType::I64);
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("LoadField outside buffer-protocol entry shape".into())
        })?;
        let in_ptr_i32 = self.lookup_param(0)?; // IR LocalGet(0) == in_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, in_ptr_i32, offset)?;
        // AOT-1: an F64 field is stored as 8 LE bytes; load it as a
        // `double`, then bit-cast to i64 bits so the operand stack stays
        // integer-typed (Option B). Routing it through `field_load_kind`
        // would yield a `FloatValue` that the shared `.into_int_value()`
        // tail below cannot consume.
        if ty == IrType::F64 {
            let name = self.next_name("loadf_f64");
            let f = self
                .builder
                .build_load(self.ctx.f64_type(), addr, &name)
                .map_err(|e| LlvmError::Codegen(format!("LoadField f64 load: {e}")))?;
            let bits = self
                .builder
                .build_bit_cast(f, self.ctx.i64_type(), &self.next_name("loadf_f64_bits"))
                .map_err(|e| LlvmError::Codegen(format!("LoadField f64 bitcast: {e}")))?
                .into_int_value();
            self.push(bits, IrType::F64);
            return Ok(());
        }
        let (llvm_ty, push_ty) = self.field_load_kind(ty)?;
        let name = self.next_name("loadf");
        let raw = self
            .builder
            .build_load(llvm_ty, addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("LoadField load: {e}")))?
            .into_int_value();
        // Widen Bool / Null (i8 on the wire) to i32 to match the IR's
        // virtual-stack convention; I32 / I64 / I8-tagged-as-Null are
        // already the correct width.
        let widened = match push_ty {
            IrType::Bool | IrType::Null => {
                let name = self.next_name("loadf_zext");
                self.builder
                    .build_int_z_extend(raw, self.ctx.i32_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadField zext: {e}")))?
            }
            _ => raw,
        };
        self.push(widened, push_ty);
        Ok(())
    }

    pub(crate) fn emit_store_field(
        &mut self,
        ip_hint: &str,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        // Phase E.1: pointer-indirect types (String / List*) route to
        // the tail-cursor protocol — bump-allocate inside the output
        // buffer's tail region, memcpy the record there, and stamp
        // the buffer-relative offset into the fixed-area slot. Comes
        // before the Phase D.1 fast-path check because the fast path
        // explicitly rejects non-I64 stores.
        if matches!(
            ty,
            IrType::String | IrType::ListInt | IrType::ListFloat | IrType::ListBool
        ) {
            return self.emit_store_field_pointer_indirect(ip_hint, offset, ty);
        }
        // Phase D.1 fast path: rewrite trailing StoreField into a
        // store against the i64 ret_slot. Only the single Int return
        // slot is supported — any other offset means the IR is past
        // the fast-path envelope (multi-field record, tail-cursor
        // payload) and we reject.
        if let Some(fast) = self.fast_path.clone() {
            if ty != IrType::I64 {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreField: only I64 returns supported, got {ty:?}"
                )));
            }
            if offset != fast.profile.ret_offset {
                return Err(LlvmError::Codegen(format!(
                    "fast-path StoreField: offset {offset} != profile.ret_offset {}",
                    fast.profile.ret_offset
                )));
            }
            let v = self.pop_int(ip_hint)?;
            self.builder
                .build_store(fast.ret_slot, v)
                .map_err(|e| LlvmError::Codegen(format!("fast StoreField ret_slot: {e}")))?;
            return Ok(());
        }
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField outside buffer-protocol entry shape".into())
        })?;
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, out_ptr_i32, offset)?;
        let v = self.pop_int(ip_hint)?;
        let store_val: BasicValueEnum<'ctx> = match ty {
            IrType::I64 => v.into(),
            IrType::I32 => v.into(),
            IrType::F64 => {
                // The IR's virtual stack carries f64 as bit-cast i64;
                // we don't see ConstF64 / Add(F64) in the Phase B
                // envelope, but a future LoadField -> StoreField pair
                // could leave an i64 on the stack tagged as F64.
                // Treat it as an i64 store; the bit-cast happens at
                // the host side.
                v.into()
            }
            IrType::Bool | IrType::Null => {
                // Narrow the i32 to i8 before storing.
                let name = self.next_name("storef_trunc");
                let narrowed = self
                    .builder
                    .build_int_truncate(v, self.ctx.i8_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreField trunc: {e}")))?;
                narrowed.into()
            }
            other => {
                return Err(LlvmError::Codegen(format!(
                    "StoreField: Phase B envelope rejects {other:?}"
                )));
            }
        };
        self.builder
            .build_store(addr, store_val)
            .map_err(|e| LlvmError::Codegen(format!("StoreField store: {e}")))?;
        Ok(())
    }

    /// Compute `arena_base + buf_ptr + offset` as an LLVM pointer.
    /// The result is a typed-stripped opaque pointer suitable for any
    /// `load` / `store` width.
    pub(crate) fn compute_buffer_addr(
        &mut self,
        arena_base_ptr: PointerValue<'ctx>,
        buf_ptr_i32: IntValue<'ctx>,
        offset: u32,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        // Widen `buf_ptr_i32` to i64 (zero-extend — wasm semantics
        // treat the i32 as an unsigned byte offset).
        let name = self.next_name("buf_ptr_zext");
        let buf_ptr64 = self
            .builder
            .build_int_z_extend(buf_ptr_i32, i64_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("buf_ptr zext: {e}")))?;
        let off_const = i32_t.const_int(u64::from(offset), false);
        let off64 = self
            .builder
            .build_int_z_extend(off_const, i64_t, "off_zext")
            .map_err(|e| LlvmError::Codegen(format!("offset zext: {e}")))?;
        let name = self.next_name("buf_off");
        let combined = self
            .builder
            .build_int_add(buf_ptr64, off64, &name)
            .map_err(|e| LlvmError::Codegen(format!("buf_ptr + offset: {e}")))?;
        // GEP from the cached arena_base pointer (which is an i8*)
        // by the combined byte offset.
        let name = self.next_name("field_addr");
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[combined], &name)
                .map_err(|e| LlvmError::Codegen(format!("field GEP: {e}")))?
        };
        Ok(addr)
    }

    pub(crate) fn field_load_kind(
        &self,
        ty: IrType,
    ) -> Result<(inkwell::types::BasicTypeEnum<'ctx>, IrType), LlvmError> {
        let pair: (inkwell::types::BasicTypeEnum<'ctx>, IrType) = match ty {
            IrType::I64 => (self.ctx.i64_type().into(), IrType::I64),
            IrType::I32 => (self.ctx.i32_type().into(), IrType::I32),
            IrType::F64 => (self.ctx.f64_type().into(), IrType::F64),
            IrType::Bool => (self.ctx.i8_type().into(), IrType::Bool),
            IrType::Null => (self.ctx.i8_type().into(), IrType::Null),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "LoadField: Phase B envelope rejects {other:?}"
                )));
            }
        };
        Ok(pair)
    }

    /// Phase 2 surface-widening: lower `Op::ReadStringLen` — pop an
    /// arena-relative i32 record pointer (String or List* header),
    /// load the leading 4-byte length prefix, zext to i64, push.
    /// Mirrors `relon-codegen-cranelift::codegen::field::emit_read_string_len`.
    ///
    /// No bounds check today (Phase B/C/D LLVM emitter doesn't emit
    /// `cond_trap`; Phase 3 wires the trap-propagation work).
    pub(crate) fn emit_read_string_len(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let ptr_i32 = self.pop_int(ip_hint)?;
        let addr = self.arena_addr_i32(ptr_i32)?;
        let name = self.next_name("rs_len");
        let len_i32 = self
            .builder
            .build_load(self.ctx.i32_type(), addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("ReadStringLen load: {e}")))?
            .into_int_value();
        let name = self.next_name("rs_len64");
        let len_i64 = self
            .builder
            .build_int_z_extend(len_i32, self.ctx.i64_type(), &name)
            .map_err(|e| LlvmError::Codegen(format!("ReadStringLen zext: {e}")))?;
        self.push(len_i64, IrType::I64);
        Ok(())
    }

    /// Phase 2 surface-widening: lower `Op::LoadStringPtr` (and its
    /// List* siblings) — `#main`-side String / List parameter loads.
    ///
    /// The IR's lowering pass emits this op whenever a `#main(String s)`
    /// (or List-typed) parameter is referenced; the buffer-protocol
    /// trampoline laid the matching record pointer (a 4-byte
    /// buffer-relative offset) at `offset` bytes inside the input
    /// record. We materialise the offset on the operand stack as an
    /// `IrType::String` (or matching List type) so downstream ops
    /// (`ReadStringLen`, `Op::Call { contains }`, list-method
    /// dispatch) see the same shape they would inside a freshly-
    /// produced literal.
    ///
    /// `IR LocalGet(0)` reads the buffer-protocol entry's `in_ptr`
    /// param (slot 1 on LLVM under `param_base = 1`); the pointer-
    /// indirect slot lives at that address plus `offset`. The
    /// resulting load is a plain i32, so we don't go through
    /// `field_load_kind`'s zext / type-tagging logic.
    ///
    /// The slot value the host marshalled into the input buffer is
    /// **input-buffer-relative** (relative to `in_ptr`, the start of the
    /// input record — `BufferBuilder` lays the tail record into the input
    /// buffer and back-patches a buffer-relative offset). Every operand-
    /// stack pointer consumer downstream (`ReadStringLen` via
    /// `arena_addr_i32`, the pointer-indirect `StoreField` /
    /// `EmitTailRecordFromAbsoluteAddr` tail-record copy, `Op::Call`
    /// stdlib helpers) treats the pointer as **arena-relative** — the
    /// same coordinate the const-pool / scratch producers push. So we
    /// rebase the loaded offset by `in_ptr` here, once at the source,
    /// instead of teaching every consumer about the param-vs-const
    /// distinction. Without this rebase, returning a list / string
    /// parameter by identity (`#main(List<Int> xs) -> List<Int> = xs`)
    /// copies the record from `arena_base + offset` (wrong region) and
    /// emits garbage; see `tests/aot_list_param_return.rs`.
    pub(crate) fn emit_load_pointer_indirect_param(
        &mut self,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen(format!(
                "Op::Load*Ptr({ty:?}) outside buffer-protocol entry shape"
            ))
        })?;
        let in_ptr_i32 = self.lookup_param(0)?; // IR LocalGet(0) == in_ptr
        let addr = self.compute_buffer_addr(arena_base_ptr, in_ptr_i32, offset)?;
        let name = self.next_name("loadptr");
        let raw = self
            .builder
            .build_load(self.ctx.i32_type(), addr, &name)
            .map_err(|e| LlvmError::Codegen(format!("Load*Ptr load: {e}")))?
            .into_int_value();
        // Rebase the input-buffer-relative slot value to an arena-relative
        // offset so the uniform `arena_base + ptr` resolution every
        // downstream consumer uses lands inside the input record.
        let name = self.next_name("loadptr_arena_rel");
        let arena_rel = self
            .builder
            .build_int_add(raw, in_ptr_i32, &name)
            .map_err(|e| LlvmError::Codegen(format!("Load*Ptr rebase: {e}")))?;
        self.push(arena_rel, ty);
        Ok(())
    }

    /// Compute `align_up(value + add, align)` as an i32 value. `align`
    /// must be a power of two (the record alignments are 4 / 8); for
    /// `align <= 1` the rounding is a no-op and the result is `value +
    /// add`. Used by the pointer-indirect record copy to resolve a
    /// record's inner payload position (`align_up(record_start + 4,
    /// align)`) from either the source or destination record start.
    pub(crate) fn align_up_const(
        &mut self,
        value: IntValue<'ctx>,
        add: u32,
        align: u32,
        label: &str,
    ) -> Result<IntValue<'ctx>, LlvmError> {
        let i32_t = self.ctx.i32_type();
        let summed = self
            .builder
            .build_int_add(
                value,
                i32_t.const_int(u64::from(add), false),
                &format!("{label}_sum"),
            )
            .map_err(|e| LlvmError::Codegen(format!("{label} add: {e}")))?;
        if align <= 1 {
            return Ok(summed);
        }
        let bumped = self
            .builder
            .build_int_add(
                summed,
                i32_t.const_int(u64::from(align - 1), false),
                &format!("{label}_bump"),
            )
            .map_err(|e| LlvmError::Codegen(format!("{label} align bump: {e}")))?;
        let mask = i32_t.const_int(u64::from(!(align - 1)), false);
        self.builder
            .build_and(bumped, mask, &format!("{label}_align"))
            .map_err(|e| LlvmError::Codegen(format!("{label} align and: {e}")))
    }

    /// Compute `arena_base + off_i32` as an LLVM pointer. Mirrors
    /// `Codegen::arena_addr` on the cranelift side — used by every
    /// `*AtAbsolute` lowering path. No bounds check (Phase E.1 retains
    /// the same "trust the IR + LLVM trap on UB" stance as Phase B).
    pub(crate) fn arena_addr_i32(
        &mut self,
        off_i32: IntValue<'ctx>,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("absolute load/store outside buffer-protocol entry shape".into())
        })?;
        let i64_t = self.ctx.i64_type();
        let i8_t = self.ctx.i8_type();
        let name = self.next_name("abs_off_zext");
        let off64 = self
            .builder
            .build_int_z_extend(off_i32, i64_t, &name)
            .map_err(|e| LlvmError::Codegen(format!("abs offset zext: {e}")))?;
        let name = self.next_name("abs_addr");
        let addr = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, arena_base_ptr, &[off64], &name)
                .map_err(|e| LlvmError::Codegen(format!("abs GEP: {e}")))?
        };
        Ok(addr)
    }

    /// Compose `base + offset` (both i32) into the absolute pointer
    /// each `Load*AtAbsolute` / `Store*AtAbsolute` op reads from.
    pub(crate) fn compose_abs_addr(
        &mut self,
        base: IntValue<'ctx>,
        offset: u32,
    ) -> Result<PointerValue<'ctx>, LlvmError> {
        let off_const = self.ctx.i32_type().const_int(u64::from(offset), false);
        let name = self.next_name("abs_compose");
        let composed = self
            .builder
            .build_int_add(base, off_const, &name)
            .map_err(|e| LlvmError::Codegen(format!("abs compose add: {e}")))?;
        self.arena_addr_i32(composed)
    }

    pub(crate) fn emit_load_at_absolute(
        &mut self,
        ip_hint: &str,
        offset: u32,
        kind: AbsLoad,
    ) -> Result<(), LlvmError> {
        let base = self.pop_int(ip_hint)?;
        let addr = self.compose_abs_addr(base, offset)?;
        match kind {
            AbsLoad::I32 => {
                let name = self.next_name("loadi32_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.i32_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI32AtAbsolute: {e}")))?
                    .into_int_value();
                self.push(v, IrType::I32);
            }
            AbsLoad::I64 => {
                let name = self.next_name("loadi64_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.i64_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI64AtAbsolute: {e}")))?
                    .into_int_value();
                self.push(v, IrType::I64);
            }
            AbsLoad::I8U => {
                let name = self.next_name("loadi8u_abs");
                let b = self
                    .builder
                    .build_load(self.ctx.i8_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI8UAtAbsolute: {e}")))?
                    .into_int_value();
                let name = self.next_name("loadi8u_zext");
                let v = self
                    .builder
                    .build_int_z_extend(b, self.ctx.i32_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadI8UAtAbsolute zext: {e}")))?;
                self.push(v, IrType::I32);
            }
            AbsLoad::F64 => {
                // Float ops are outside the present W3/W4 envelope; we
                // still accept LoadF64AtAbsolute to keep the dispatcher
                // exhaustive. The stack carries the bit-cast i64.
                let name = self.next_name("loadf64_abs");
                let v = self
                    .builder
                    .build_load(self.ctx.f64_type(), addr, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadF64AtAbsolute: {e}")))?;
                // Bit-cast to i64 to feed the int-typed virtual stack.
                let i64_t = self.ctx.i64_type();
                let name = self.next_name("loadf64_bitcast");
                let bits = self
                    .builder
                    .build_bit_cast(v, i64_t, &name)
                    .map_err(|e| LlvmError::Codegen(format!("LoadF64 bitcast: {e}")))?
                    .into_int_value();
                self.push(bits, IrType::F64);
            }
        }
        Ok(())
    }

    pub(crate) fn emit_store_at_absolute(
        &mut self,
        ip_hint: &str,
        offset: u32,
        kind: AbsStore,
    ) -> Result<(), LlvmError> {
        // Stack: `[base, value]` — top is the value, below it is the
        // base. Mirrors cranelift's pop order.
        let value = self.pop_int(ip_hint)?;
        let base = self.pop_int(ip_hint)?;
        let addr = self.compose_abs_addr(base, offset)?;
        match kind {
            AbsStore::I32 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI32AtAbsolute: {e}")))?;
            }
            AbsStore::I64 => {
                self.builder
                    .build_store(addr, value)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI64AtAbsolute: {e}")))?;
            }
            AbsStore::I8 => {
                // Narrow the i32 value to i8 before the store.
                let name = self.next_name("storei8_trunc");
                let narrowed = self
                    .builder
                    .build_int_truncate(value, self.ctx.i8_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI8AtAbsolute trunc: {e}")))?;
                self.builder
                    .build_store(addr, narrowed)
                    .map_err(|e| LlvmError::Codegen(format!("StoreI8AtAbsolute: {e}")))?;
            }
            AbsStore::F64 => {
                // The IR's virtual stack carries f64 as bit-cast i64;
                // bit-cast back before the store so the destination
                // bytes match the wasm f64 wire layout.
                let name = self.next_name("storef64_bitcast");
                let f = self
                    .builder
                    .build_bit_cast(value, self.ctx.f64_type(), &name)
                    .map_err(|e| LlvmError::Codegen(format!("StoreF64 bitcast: {e}")))?;
                self.builder
                    .build_store(addr, f)
                    .map_err(|e| LlvmError::Codegen(format!("StoreF64AtAbsolute: {e}")))?;
            }
        }
        Ok(())
    }

    /// Lower `Op::MemcpyAtAbsolute`. Stack: `[dst, src, len]`. Calls
    /// LLVM's `llvm.memcpy.p0.p0.i64` intrinsic with both pointers
    /// resolved through `arena_base`.
    pub(crate) fn emit_memcpy_at_absolute(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let len = self.pop_int(ip_hint)?;
        let src = self.pop_int(ip_hint)?;
        let dst = self.pop_int(ip_hint)?;
        let dst_ptr = self.arena_addr_i32(dst)?;
        let src_ptr = self.arena_addr_i32(src)?;
        // `inkwell`'s `build_memcpy` requires the length to be the
        // pointer-width int. Widen our i32 length to i64 (zero-extend).
        let i64_t = self.ctx.i64_type();
        let len64 = self
            .builder
            .build_int_z_extend(len, i64_t, "memcpy_len_zext")
            .map_err(|e| LlvmError::Codegen(format!("memcpy len zext: {e}")))?;
        // Pick a 1-byte alignment hint — the inner records aren't
        // guaranteed > 1-byte aligned (string headers land on 4-byte
        // boundaries but their payload follows immediately). The LLVM
        // optimiser will refine when it can prove a tighter bound.
        self.builder
            .build_memcpy(dst_ptr, 1, src_ptr, 1, len64)
            .map_err(|e| LlvmError::Codegen(format!("MemcpyAtAbsolute build: {e}")))?;
        Ok(())
    }

    /// Bump-allocate `size_v` (i32) bytes inside the arena's scratch
    /// region. Pushes the pre-bump cursor as an arena-relative i32
    /// offset onto the virtual stack — same shape as cranelift's
    /// `emit_alloc_scratch`.
    pub(crate) fn emit_alloc_scratch_common(
        &mut self,
        size_v: IntValue<'ctx>,
    ) -> Result<(), LlvmError> {
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen(
                "AllocScratch outside buffer-protocol entry shape (no state ptr)".into(),
            )
        })?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();

        // GEP-then-load helpers. We hand-roll the i8-offset GEPs
        // because the inkwell wrappers expect a struct field accessor.
        let cursor_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_CURSOR), false)],
                    "scratch_cursor_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_cursor GEP: {e}")))?
        };
        let cur = self
            .builder
            .build_load(i32_t, cursor_gep, "scratch_cursor")
            .map_err(|e| LlvmError::Codegen(format!("scratch_cursor load: {e}")))?
            .into_int_value();
        let base_gep = unsafe {
            self.builder
                .build_in_bounds_gep(
                    i8_t,
                    state_ptr,
                    &[i32_t.const_int(u64::from(ARENA_STATE_OFFSET_SCRATCH_BASE), false)],
                    "scratch_base_gep",
                )
                .map_err(|e| LlvmError::Codegen(format!("scratch_base GEP: {e}")))?
        };
        let scratch_base = self
            .builder
            .build_load(i32_t, base_gep, "scratch_base")
            .map_err(|e| LlvmError::Codegen(format!("scratch_base load: {e}")))?
            .into_int_value();

        // Returned arena-relative offset = scratch_base + cur.
        let off = self
            .builder
            .build_int_add(scratch_base, cur, "scratch_off")
            .map_err(|e| LlvmError::Codegen(format!("scratch off add: {e}")))?;
        // New cursor = cur + size.
        let new_cur = self
            .builder
            .build_int_add(cur, size_v, "scratch_new_cur")
            .map_err(|e| LlvmError::Codegen(format!("scratch cur bump: {e}")))?;
        self.builder
            .build_store(cursor_gep, new_cur)
            .map_err(|e| LlvmError::Codegen(format!("scratch cursor store: {e}")))?;
        self.push(off, IrType::I32);
        Ok(())
    }

    pub(crate) fn emit_alloc_scratch_static(&mut self, size_bytes: u32) -> Result<(), LlvmError> {
        let size_v = self.ctx.i32_type().const_int(u64::from(size_bytes), false);
        self.emit_alloc_scratch_common(size_v)
    }

    pub(crate) fn emit_alloc_scratch_dyn(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let size = self.pop_int(ip_hint)?;
        self.emit_alloc_scratch_common(size)
    }

    /// Lower `Op::StoreField { ty }` for pointer-indirect types
    /// (`String`, `ListInt`, `ListFloat`, `ListBool`). Pops the source
    /// arena offset, copies the `[len:u32 LE][payload]` record into
    /// the output buffer's tail region (`out_ptr + tail_cursor`),
    /// writes `tail_cursor` (buffer-relative offset of the new record)
    /// into the fixed-area slot at `offset`, and bumps `tail_cursor`.
    /// Mirrors cranelift's `emit_store_pointer_indirect`.
    pub(crate) fn emit_store_field_pointer_indirect(
        &mut self,
        ip_hint: &str,
        offset: u32,
        ty: IrType,
    ) -> Result<(), LlvmError> {
        let arena_base_ptr = self.arena_base_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField (pointer-indirect) outside buffer entry".into())
        })?;
        let state_ptr = self.state_ptr.ok_or_else(|| {
            LlvmError::Codegen("StoreField (pointer-indirect): missing state ptr".into())
        })?;
        let src_off_i32 = self.pop_int(ip_hint)?;
        let i32_t = self.ctx.i32_type();
        let i8_t = self.ctx.i8_type();
        // Read the record's `[len: u32]` header to size the memcpy.
        let src_abs = self.arena_addr_i32(src_off_i32)?;
        let len_i32 = self
            .builder
            .build_load(i32_t, src_abs, "ptr_indirect_len")
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect len load: {e}")))?
            .into_int_value();
        let record_size = match ty {
            IrType::String => {
                let four = i32_t.const_int(4, false);
                self.builder
                    .build_int_add(len_i32, four, "string_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("String record_size: {e}")))?
            }
            IrType::ListInt | IrType::ListFloat => {
                // record_size = 8 + 8 * element_count.
                let three = i32_t.const_int(3, false);
                let shifted = self
                    .builder
                    .build_left_shift(len_i32, three, "list_shl")
                    .map_err(|e| LlvmError::Codegen(format!("list shl: {e}")))?;
                let eight = i32_t.const_int(8, false);
                self.builder
                    .build_int_add(shifted, eight, "list_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("list record_size: {e}")))?
            }
            IrType::ListBool => {
                let four = i32_t.const_int(4, false);
                self.builder
                    .build_int_add(len_i32, four, "listbool_recsize")
                    .map_err(|e| LlvmError::Codegen(format!("listbool record_size: {e}")))?
            }
            _ => {
                return Err(LlvmError::Codegen(format!(
                    "emit_store_field_pointer_indirect: unsupported {ty:?}"
                )));
            }
        };
        // Pick the alignment for the tail bump. String / ListBool stay
        // 4-aligned (the leading u32 length); ListInt / ListFloat need
        // 8 so the i64 / f64 payload that follows is aligned.
        let align: u32 = match ty {
            IrType::String | IrType::ListBool => 4,
            IrType::ListInt | IrType::ListFloat => 8,
            _ => unreachable!(),
        };
        // Tail bump: aligned = align_up(cur, align); new_cur = aligned + record_size.
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
            let mask_val = !(align - 1);
            let mask = i32_t.const_int(u64::from(mask_val), false);
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
        // Write the destination record at `out_ptr + aligned`.
        //
        // The record's *inner* padding is position-dependent: the host
        // protocol lays the payload at `align_up(record_start + 4,
        // align)`, so the gap between the 4-byte `[len]` header and the
        // payload is `align_up(record_start + 4, align) - record_start -
        // 4` — which differs between the source record (whatever offset
        // the input marshaller / const-pool put it at) and the freshly-
        // aligned destination slot. A verbatim `memcpy(record_size)` from
        // the source therefore drags the *source's* pad geometry into the
        // destination and misaligns the payload whenever the two record
        // starts have different `% align` residues (e.g. a `List<Int>`
        // input arg lands its record 4-aligned-but-not-8, payload at
        // header+4; the 8-aligned output slot expects payload at
        // header+8). So copy the `[len]` header and the payload
        // *separately*, reading the payload from the source's actual
        // payload position and writing it to the destination's — the pad
        // is recomputed on each side rather than copied.
        let out_ptr_i32 = self.lookup_param(2)?; // IR LocalGet(2) == out_ptr
        let dst_off = self
            .builder
            .build_int_add(out_ptr_i32, aligned, "ptr_indirect_dst_off")
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect dst off: {e}")))?;
        let dst_ptr = self.arena_addr_i32(dst_off)?;
        let i64_t = self.ctx.i64_type();
        let _ = arena_base_ptr;
        // Header: store the `[len: u32]` prefix at the destination record
        // start (`dst_off + 0`).
        self.builder
            .build_store(dst_ptr, len_i32)
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect len store: {e}")))?;
        // Payload byte count: String / ListBool are 1 byte/element,
        // ListInt / ListFloat are 8.
        let payload_bytes = match ty {
            IrType::String | IrType::ListBool => len_i32,
            IrType::ListInt | IrType::ListFloat => self
                .builder
                .build_left_shift(len_i32, i32_t.const_int(3, false), "payload_shl")
                .map_err(|e| LlvmError::Codegen(format!("payload shl: {e}")))?,
            _ => unreachable!("record_size match already rejected other types"),
        };
        // Source payload offset = align_up(src_off + 4, align). Recompute
        // it from the (arena-relative) source record start rather than
        // assuming a fixed header+pad — see the comment above.
        let src_payload_off = self.align_up_const(src_off_i32, 4, align, "src_payload")?;
        let src_payload_ptr = self.arena_addr_i32(src_payload_off)?;
        // Destination payload offset = align_up(dst_off + 4, align).
        let dst_payload_off = self.align_up_const(dst_off, 4, align, "dst_payload")?;
        let dst_payload_ptr = self.arena_addr_i32(dst_payload_off)?;
        let payload64 = self
            .builder
            .build_int_z_extend(payload_bytes, i64_t, "ptr_indirect_payload64")
            .map_err(|e| LlvmError::Codegen(format!("payload64 zext: {e}")))?;
        self.builder
            .build_memcpy(dst_payload_ptr, align, src_payload_ptr, 1, payload64)
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect payload memcpy: {e}")))?;
        // Store `aligned` (buffer-relative offset of the new record)
        // into the fixed-area slot at `out_ptr + offset`.
        let slot_off = self
            .builder
            .build_int_add(
                out_ptr_i32,
                i32_t.const_int(u64::from(offset), false),
                "ptr_indirect_slot_off",
            )
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect slot off: {e}")))?;
        let slot_addr = self.arena_addr_i32(slot_off)?;
        self.builder
            .build_store(slot_addr, aligned)
            .map_err(|e| LlvmError::Codegen(format!("ptr-indirect slot store: {e}")))?;
        // Flag the body so the buffer-protocol epilogue returns the
        // post-bump tail cursor.
        self.needs_tail_cursor = true;
        Ok(())
    }
}
