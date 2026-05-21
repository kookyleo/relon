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
}
