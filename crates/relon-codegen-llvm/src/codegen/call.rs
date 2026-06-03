//! `Op`-family: call dispatch.
//!
//! User-defined `Call` (direct LLVM call), bundled-stdlib inline `Call`,
//! and the `llvm.trap` guard call. `CallNative` / `CheckCap` stay in the
//! `unsupported` set in `super::lower_op` for Phase 0b.

use inkwell::values::{
    BasicMetadataValueEnum, BasicValueEnum,
};

use relon_ir::ir::IrType;

use crate::error::LlvmError;

use super::*;

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    /// Phase E.2 multi-function dispatch: lower `Op::Call`.
    ///
    /// The IR's `fn_index` is split as `[0..stdlib_count) = bundled
    /// stdlib body` / `[stdlib_count..) = user-defined sibling`. The
    /// LLVM emitter currently only routes the sibling slice — stdlib
    /// inlining stays parked on the cranelift backend. A stdlib call
    /// surfaces `LlvmError::Codegen` so the host can fall back.
    pub(crate) fn emit_call(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib_count = relon_ir::stdlib::stdlib_function_count();
        if fn_index < stdlib_count {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to stdlib fn_index={fn_index} not yet supported in LLVM AOT \
                 (cranelift inlines bundled stdlib bodies; LLVM path widens with #278)"
            )));
        }
        let helper_idx = fn_index - stdlib_count;
        let callee = match self.helper_table.as_ref().and_then(|t| t.get(&helper_idx)) {
            Some(fv) => *fv,
            None => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx} (fn_index={fn_index}, stdlib_count={stdlib_count}) \
                     not in helper_table — module may be missing the function"
                )));
            }
        };

        // Sanity check arity against the declared signature.
        if callee.count_params() as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: callee has {} LLVM params, IR declares {}",
                callee.count_params(),
                param_tys.len()
            )));
        }
        if arg_count as usize != param_tys.len() {
            return Err(LlvmError::Codegen(format!(
                "Op::Call helper_idx={helper_idx}: arg_count={arg_count} != param_tys.len()={}",
                param_tys.len()
            )));
        }

        // Pop the arguments off the operand stack — last-pushed value
        // is the last param.
        let mut args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop_int(ip_hint)?.into());
        }
        args.reverse();

        // Adjust each arg's LLVM type to match the callee's declared
        // param: widen / truncate i32 <-> i64 as needed. The IR's
        // stack-machine semantics keep types tagged but the wasm slot
        // widening can leave a Bool-as-i32 in front of an I64 callee
        // param. We re-coerce here to match the helper's signature.
        for (i, (slot, want_ty)) in args.iter_mut().zip(param_tys.iter()).enumerate() {
            let arg_val = match slot {
                BasicMetadataValueEnum::IntValue(v) => *v,
                other => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: expected IntValue, got {other:?}"
                    )));
                }
            };
            let want_width = match *want_ty {
                IrType::I64 => 64,
                IrType::I32
                | IrType::Bool
                | IrType::Null
                | IrType::String
                | IrType::ListInt
                | IrType::ListFloat
                | IrType::ListBool
                | IrType::ListString
                | IrType::ListSchema
                | IrType::Closure => 32,
                IrType::F64 => {
                    return Err(LlvmError::Codegen(format!(
                        "Op::Call arg #{i}: F64 param not yet supported in Phase E.2"
                    )));
                }
            };
            let have_width = arg_val.get_type().get_bit_width();
            if have_width != want_width {
                let target_ty = if want_width == 64 {
                    self.ctx.i64_type()
                } else {
                    self.ctx.i32_type()
                };
                let coerced = if have_width < want_width {
                    self.builder
                        .build_int_z_extend(arg_val, target_ty, "call_arg_zext")
                        .map_err(|e| LlvmError::Codegen(format!("call arg zext: {e}")))?
                } else {
                    self.builder
                        .build_int_truncate(arg_val, target_ty, "call_arg_trunc")
                        .map_err(|e| LlvmError::Codegen(format!("call arg trunc: {e}")))?
                };
                *slot = coerced.into();
            }
        }

        let name = self.next_name("call_ret");
        let call_site = self
            .builder
            .build_call(callee, &args, &name)
            .map_err(|e| LlvmError::Codegen(format!("Op::Call build_call: {e}")))?;
        let ret_val = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(v) => v,
            inkwell::values::ValueKind::Instruction(_) => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned void; Phase E.2 envelope expects a typed return"
                )));
            }
        };
        let ret_int = match ret_val {
            BasicValueEnum::IntValue(v) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call helper_idx={helper_idx}: callee returned {other:?}, expected IntValue"
                )));
            }
        };
        self.push(ret_int, ret_ty);
        Ok(())
    }

    /// Phase E.2: emit a call to the `llvm.trap` intrinsic. The
    /// intrinsic must be pre-declared on the module via
    /// [`declare_llvm_trap`] before the first guard fires; the
    /// declaration is cached on the `Emit` so repeated div / mod
    /// guards share one `FunctionValue`. The `op_hint` is used only
    /// for diagnostic naming on the build_call site.
    pub(crate) fn emit_llvm_trap_call(&mut self, op_hint: &str) -> Result<(), LlvmError> {
        let trap_fn = self.llvm_trap_fn.ok_or_else(|| {
            LlvmError::Codegen(format!(
                "{op_hint}: llvm.trap intrinsic missing — emit_module_funcs forgot to declare it"
            ))
        })?;
        let name = self.next_name("trap_call");
        self.builder
            .build_call(trap_fn, &[], &name)
            .map_err(|e| LlvmError::Codegen(format!("{op_hint} llvm.trap build_call: {e}")))?;
        Ok(())
    }

    /// Lower `Op::Call { fn_index, ... }` by inlining the bundled
    /// stdlib body. Mirrors cranelift's `emit_call_stdlib` — pop the
    /// args, set up an inline frame with an exit-block target, lower
    /// the callee body recursively against the inline frame, then
    /// continue at the exit block with the loaded result on the stack.
    pub(crate) fn emit_call_stdlib(
        &mut self,
        ip_hint: &str,
        fn_index: u32,
        arg_count: u32,
        param_tys: &[IrType],
        ret_ty: IrType,
    ) -> Result<(), LlvmError> {
        let stdlib = relon_ir::stdlib::builtin_stdlib();
        let callee = stdlib.get(fn_index as usize).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "Op::Call fn_index {fn_index} outside bundled stdlib (max {})",
                stdlib.len()
            ))
        })?;
        if callee.params.len() != arg_count as usize {
            return Err(LlvmError::Codegen(format!(
                "Op::Call to `{}` declares {arg_count} args but callee has {}",
                callee.name,
                callee.params.len()
            )));
        }
        for (i, (declared, expected)) in callee.params.iter().zip(param_tys.iter()).enumerate() {
            if declared != expected {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call to `{}` arg #{i}: callee expects {declared:?}, IR tags {expected:?}",
                    callee.name
                )));
            }
        }
        // Pop args in reverse so `params[i]` is the i-th declared arg.
        let mut args: Vec<TypedValue<'ctx>> = Vec::with_capacity(arg_count as usize);
        for _ in 0..arg_count {
            args.push(self.pop(ip_hint)?);
        }
        args.reverse();

        // Pick a let_offset window past any active let slots so the
        // callee's `LetSet 0` lands at `let_offset + 0` and never
        // clashes with the caller's bindings. Cranelift uses
        // `max(idx) + 1`; we do the same by inspecting `let_slots`.
        let let_offset = self
            .let_slots
            .keys()
            .copied()
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);

        // Alloca for the callee's return value. The callee's
        // `Op::Return` stores into this slot then jumps to `exit_bb`;
        // the caller-side load below pushes the value back on the
        // virtual stack.
        let ret_llvm_ty: inkwell::types::BasicTypeEnum<'ctx> = match ret_ty {
            IrType::I64 => self.ctx.i64_type().into(),
            IrType::I32
            | IrType::Bool
            | IrType::Null
            | IrType::String
            | IrType::ListInt
            | IrType::ListFloat
            | IrType::ListBool => self.ctx.i32_type().into(),
            other => {
                return Err(LlvmError::Codegen(format!(
                    "Op::Call ret_ty {other:?} unsupported in inline frame"
                )));
            }
        };
        // Allocate the ret slot in the function's entry block so it
        // stays out of any loop body; mem2reg promotes it on -O2/-O3.
        let entry_bb = self.func.get_first_basic_block().ok_or_else(|| {
            LlvmError::Codegen("emit_call_stdlib: function has no entry block".into())
        })?;
        let cur = self.builder.get_insert_block();
        if let Some(first_instr) = entry_bb.get_first_instruction() {
            self.builder.position_before(&first_instr);
        } else {
            self.builder.position_at_end(entry_bb);
        }
        let ret_slot = self
            .builder
            .build_alloca(ret_llvm_ty, "call_ret_slot")
            .map_err(|e| LlvmError::Codegen(format!("call ret_slot alloca: {e}")))?;
        if let Some(bb) = cur {
            self.builder.position_at_end(bb);
        }

        let exit_bb = self.ctx.append_basic_block(self.func, "call_exit");
        let frame = InlineFrame {
            params: args,
            let_offset,
            ret_slot,
            ret_ty,
            exit_bb,
        };
        self.inline_frames.push(frame);
        let body = callee.body_owned();
        let result = self.lower_body(&body);
        // Always pop the frame before returning the error so the emit
        // state stays consistent on failure.
        self.inline_frames.pop();
        result?;

        // After the inline body finishes the current block has either
        // hit `Op::Return` (which terminated via `br exit_bb`) or fell
        // through. If it fell through, branch to exit_bb so the
        // load + push below has a single in-edge.
        let cur_terminated = self
            .builder
            .get_insert_block()
            .and_then(|bb| bb.get_terminator())
            .is_some();
        if !cur_terminated {
            self.builder
                .build_unconditional_branch(exit_bb)
                .map_err(|e| LlvmError::Codegen(format!("inline call fallthrough: {e}")))?;
        }
        // Position at the exit block and load the result.
        self.builder.position_at_end(exit_bb);
        let name = self.next_name("call_ret_load");
        let v = self
            .builder
            .build_load(ret_llvm_ty, ret_slot, &name)
            .map_err(|e| LlvmError::Codegen(format!("inline call ret load: {e}")))?
            .into_int_value();
        self.push(v, ret_ty);
        Ok(())
    }
}
