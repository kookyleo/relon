//! Closure-construction + dispatch helpers for [`super::Codegen`].
//!
//! Closure handles are 8-byte scratch records:
//!
//!   `[fn_table_idx: u32 LE][captures_ptr: u32 LE]`
//!
//! `Op::MakeClosure` lowers to a fresh scratch allocation that
//! materialises the handle (and, when `captures_size > 0`, a
//! separate captures struct) and pushes the handle's arena-relative
//! pointer. `Op::CallClosure` consumes that pointer, resolves the
//! host fn through `state.closure_table_base[fn_table_idx]`, and
//! invokes it through an indirect call with the prepended
//! `(state, captures_ptr, args...)` signature.
//!
//! Both helpers rely on [`super::Codegen::emit_alloc_scratch`] /
//! [`super::Codegen::arena_addr`] (in `memory.rs`) for the per-handle
//! allocations + address arithmetic.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::I32;
use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlags, Signature, Value as CValue};
use cranelift_codegen::isa::CallConv;

use relon_ir::ir::IrType;

use crate::error::CraneliftError;
use crate::sandbox::TrapKind;

use super::ir_ty_to_cl;

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Lower `Op::MakeClosure { fn_table_idx, captures, captures_size }`.
    /// Stage 5 Phase C.4.
    ///
    /// Closure handle layout (8 bytes total):
    ///   `[fn_table_idx: u32 LE][captures_ptr: u32 LE]`
    ///
    /// Layout in scratch:
    ///   1. Alloc 8 bytes for the handle (arena-relative ptr →
    ///      `handle_ptr`).
    ///   2. If `captures_size > 0`: alloc `captures_size` bytes for
    ///      the captures struct (→ `captures_ptr`); write each capture
    ///      from its let-local into the struct at the declared offset.
    ///   3. Store `fn_table_idx` at `handle_ptr + 0`.
    ///   4. Store `captures_ptr` (or 0) at `handle_ptr + 4`.
    ///   5. Push `handle_ptr` as i32 onto the operand stack.
    pub(super) fn emit_make_closure(
        &mut self,
        fn_table_idx: u32,
        captures: &[relon_ir::ir::ClosureCapture],
        captures_size: u32,
    ) -> Result<(), CraneliftError> {
        // 1. Alloc 8 bytes for the handle.
        let handle_size = self.builder.ins().iconst(I32, 8);
        self.emit_alloc_scratch(handle_size)?;
        let handle_ptr = self.pop()?;

        // 2. Alloc captures struct if non-empty.
        let captures_ptr = if captures_size > 0 {
            let cs = self.builder.ins().iconst(I32, i64::from(captures_size));
            self.emit_alloc_scratch(cs)?;
            self.pop()?
        } else {
            self.builder.ins().iconst(I32, 0)
        };

        // 3. Store fn_table_idx at handle_ptr + 0.
        let fn_idx_v = self.builder.ins().iconst(I32, i64::from(fn_table_idx));
        // Use the StoreI32AtAbsolute pattern: arena_base + handle_ptr.
        let abs_handle = self.arena_addr(handle_ptr, 8)?;
        self.builder
            .ins()
            .store(MemFlags::trusted(), fn_idx_v, abs_handle, 0);
        // 4. Store captures_ptr at handle_ptr + 4.
        self.builder
            .ins()
            .store(MemFlags::trusted(), captures_ptr, abs_handle, 4);

        // 5. Write each capture from its let-local into the captures
        //    struct.
        if captures_size > 0 {
            let captures_abs = self.arena_addr(captures_ptr, captures_size)?;
            for cap in captures {
                let mapped_idx = self.remap_let_idx(cap.let_idx);
                // Determine the captured value. If the let-slot is
                // already bound, read it (ordinary capture). Otherwise
                // this is a self-recursive capture: the lowering pass
                // emits `MakeClosure` before the matching
                // `LetSet { idx: mapped_idx, ty: Closure }`, so the slot
                // does not exist yet. The captured value is the closure
                // handle being built right here — the very value the
                // upcoming `LetSet` will store — so we stamp
                // `handle_ptr` into the slot, forming a value cycle
                // (an i32 arena offset, not a borrow, so it is safe to
                // self-reference). This mirrors the LLVM backend
                // (`emitter.rs` `emit_make_closure`). A not-yet-bound
                // capture is only legal for `Closure`-typed captures
                // (any other type referring to a not-yet-bound let-local
                // is an impossible source shape and signals a lowering
                // bug).
                let value = if self.let_is_bound(mapped_idx) {
                    self.get_let(mapped_idx, cap.ty)?
                } else {
                    if cap.ty != IrType::Closure {
                        return Err(CraneliftError::Codegen(format!(
                            "MakeClosure capture let_idx={mapped_idx} not yet bound but ty={:?} (expected Closure for self-recursion)",
                            cap.ty
                        )));
                    }
                    handle_ptr
                };
                let offset = i32::try_from(cap.offset).map_err(|_| {
                    CraneliftError::Codegen(format!(
                        "MakeClosure capture offset {} exceeds i32 range",
                        cap.offset
                    ))
                })?;
                self.builder
                    .ins()
                    .store(MemFlags::trusted(), value, captures_abs, offset);
            }
        }

        // 6. Push the handle_ptr onto the operand stack as the Closure
        //    i32 value.
        self.push(handle_ptr);
        Ok(())
    }

    /// Lower `Op::CallClosure { param_tys, ret_ty }`. Stage 5 Phase C.4.
    ///
    /// Stack discipline: `[Closure, arg0, arg1, ...] -> [ret_ty]`. We
    /// pop the user-visible args (in reverse), pop the closure
    /// handle, materialise the captures_ptr + fn_table_idx from the
    /// handle, look up the host fn pointer through
    /// `state.closure_table_base[fn_table_idx]`, then `call_indirect`
    /// with the prepended `(state, captures_ptr, args...)` signature.
    pub(super) fn emit_call_closure(
        &mut self,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), CraneliftError> {
        // Pop user args in reverse.
        let mut user_args: Vec<CValue> = Vec::with_capacity(param_tys.len());
        for _ in 0..param_tys.len() {
            user_args.push(self.pop()?);
        }
        user_args.reverse();

        // Pop the closure handle (arena-relative i32 ptr).
        let handle_ptr = self.pop()?;

        // Load fn_table_idx + captures_ptr through the handle.
        let abs_handle = self.arena_addr(handle_ptr, 8)?;
        let fn_table_idx = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), abs_handle, 0);
        let captures_ptr = self
            .builder
            .ins()
            .load(I32, MemFlags::trusted(), abs_handle, 4);

        // Look up host fn pointer through
        // state.closure_table_base[fn_table_idx]. Each slot is a
        // `usize` (host pointer size).
        let table_base = self.builder.ins().load(
            self.pointer_ty,
            MemFlags::trusted(),
            self.state_ptr,
            crate::sandbox::STATE_OFFSET_CLOSURE_TABLE_BASE,
        );
        let idx_p = self.builder.ins().uextend(self.pointer_ty, fn_table_idx);
        let stride_bits = match self.pointer_ty.bits() {
            64 => 3, // log2(8) = 3
            32 => 2, // log2(4) = 2
            _ => {
                return Err(CraneliftError::Codegen(
                    "unsupported pointer width for closure table".into(),
                ))
            }
        };
        let off = self.builder.ins().ishl_imm(idx_p, stride_bits);
        let slot_addr = self.builder.ins().iadd(table_base, off);
        let fn_ptr = self
            .builder
            .ins()
            .load(self.pointer_ty, MemFlags::trusted(), slot_addr, 0);
        // Null-check the resolved fn pointer (defensive: a
        // misconfigured closure_table_base would point at zero-filled
        // memory; a null call_indirect would segfault).
        let zero = self.builder.ins().iconst(self.pointer_ty, 0);
        let cmp = self.builder.ins().icmp(IntCC::Equal, fn_ptr, zero);
        self.cond_trap(cmp, TrapKind::CapabilityDenied);

        // Build call signature: (state, captures_ptr, params...) -> ret_ty.
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(self.pointer_ty));
        sig.params.push(AbiParam::new(I32));
        for ty in param_tys {
            sig.params.push(AbiParam::new(ir_ty_to_cl(*ty)?));
        }
        if !matches!(ret_ty, IrType::Null) {
            sig.returns.push(AbiParam::new(ir_ty_to_cl(ret_ty)?));
        }
        let sig_ref = self.builder.import_signature(sig);

        // Assemble args: [state, captures_ptr, user_args...].
        let mut call_args: Vec<CValue> = Vec::with_capacity(user_args.len() + 2);
        call_args.push(self.state_ptr);
        call_args.push(captures_ptr);
        call_args.extend(user_args);

        let inst = self
            .builder
            .ins()
            .call_indirect(sig_ref, fn_ptr, &call_args);

        if !matches!(ret_ty, IrType::Null) {
            let r = self.builder.inst_results(inst)[0];
            self.push(r);
        }
        Ok(())
    }
}
