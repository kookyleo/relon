//! IR → LLVM IR lowering (Phase A bootstrap).
//!
//! The emitter walks `relon-ir`'s stack-machine op stream and
//! materialises an LLVM function that mirrors the cranelift
//! crate's legacy-i64 envelope: every parameter is `i64`, return is
//! `i64`, and the body is a single basic block. The supported op
//! set is intentionally narrow:
//!
//! - `Op::ConstI64` / `Op::ConstI32` / `Op::ConstBool` — push a
//!   compile-time constant. Bool / i32 are widened to i64 on push
//!   to match the legacy-i64 calling convention.
//! - `Op::LocalGet(idx)` — push the matching function parameter.
//! - `Op::Add(IrType::I64)` / `Op::Sub(IrType::I64)` /
//!   `Op::Mul(IrType::I64)` — pop two i64s, push the result. Phase A
//!   uses wrap-on-overflow arithmetic; the trap-on-overflow flavour
//!   the cranelift crate emits today moves in alongside the
//!   `__relon_trap` helper-call surface in Phase B.
//! - `Op::Return` — pop the stack top and `ret` it.
//!
//! Mismatches against this op set surface as
//! [`crate::LlvmError::Codegen`]. The bootstrap test exercises only
//! the supported shapes; the wider IR corpus stays parked on the
//! cranelift backend.

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module as LlvmModule;
use inkwell::values::{BasicValueEnum, FunctionValue, IntValue};

use relon_ir::ir::{Func, IrType, Op, TaggedOp};

use crate::error::LlvmError;

/// Canonical export name the entry function uses in the emitted LLVM
/// module. The evaluator side `dlsym`s / `get_function`s against this
/// symbol after JIT finalize, so renaming it requires touching both
/// crates simultaneously.
pub(crate) const ENTRY_SYMBOL: &str = "relon_llvm_entry";

/// Emit a Phase-A `(I64...) -> I64` function into `module` from the
/// supplied [`Func`]. The function is exported as
/// [`ENTRY_SYMBOL`] so the runtime can resolve it through inkwell's
/// `ExecutionEngine::get_function`.
///
/// Phase A keeps the validation envelope narrow on purpose: anything
/// outside the legacy-i64 shape or the supported op set surfaces as
/// [`LlvmError::UnsupportedSignature`] / [`LlvmError::Codegen`].
/// The relon facade's `BackendError::LlvmAot` carrier then lets the
/// host fall back to the cranelift backend without crashing.
pub(crate) fn emit_function<'ctx>(
    ctx: &'ctx Context,
    module: &LlvmModule<'ctx>,
    func: &Func,
) -> Result<FunctionValue<'ctx>, LlvmError> {
    // Validate the legacy-i64 envelope up-front so a downstream
    // builder error doesn't bury the real cause (wrong param type).
    for (i, p) in func.params.iter().enumerate() {
        if *p != IrType::I64 {
            return Err(LlvmError::UnsupportedSignature(format!(
                "llvm-aot Phase A: param #{i} is {p:?}, expected I64"
            )));
        }
    }
    if func.ret != IrType::I64 {
        return Err(LlvmError::UnsupportedSignature(format!(
            "llvm-aot Phase A: return is {:?}, expected I64",
            func.ret
        )));
    }

    let i64_t = ctx.i64_type();
    let param_types: Vec<inkwell::types::BasicMetadataTypeEnum<'ctx>> =
        (0..func.params.len()).map(|_| i64_t.into()).collect();
    let fn_type = i64_t.fn_type(&param_types, false);
    let llvm_fn = module.add_function(ENTRY_SYMBOL, fn_type, None);

    let entry_bb = ctx.append_basic_block(llvm_fn, "entry");
    let builder = ctx.create_builder();
    builder.position_at_end(entry_bb);

    let mut state = EmitState {
        ctx,
        builder: &builder,
        func: llvm_fn,
        stack: Vec::with_capacity(8),
    };

    for (ip, tagged) in func.body.iter().enumerate() {
        state.emit_op(ip, tagged)?;
    }

    // A well-formed body ends with `Op::Return` which already issued
    // an `ret` instruction. If the producer omitted it the LLVM
    // verifier would catch the missing terminator; we surface a
    // clearer message here so the test failure points back at the
    // IR rather than at inkwell's verifier output.
    if llvm_fn
        .get_last_basic_block()
        .and_then(|bb| bb.get_terminator())
        .is_none()
    {
        return Err(LlvmError::Codegen(format!(
            "function body did not end with Op::Return (body len = {})",
            func.body.len()
        )));
    }

    Ok(llvm_fn)
}

/// Per-function emission state. Holds the inkwell context + builder
/// borrow alongside the operand stack the IR's stack machine drives.
struct EmitState<'ctx, 'b> {
    ctx: &'ctx Context,
    builder: &'b Builder<'ctx>,
    func: FunctionValue<'ctx>,
    stack: Vec<IntValue<'ctx>>,
}

impl<'ctx, 'b> EmitState<'ctx, 'b> {
    fn push(&mut self, v: IntValue<'ctx>) {
        self.stack.push(v);
    }

    fn pop(&mut self, ip: usize, op_name: &str) -> Result<IntValue<'ctx>, LlvmError> {
        self.stack.pop().ok_or_else(|| {
            LlvmError::Codegen(format!(
                "stack underflow at ip={ip} ({op_name}): producer emitted an Op with no matching push"
            ))
        })
    }

    fn emit_op(&mut self, ip: usize, tagged: &TaggedOp) -> Result<(), LlvmError> {
        match &tagged.op {
            Op::ConstI64(v) => {
                let c = self.ctx.i64_type().const_int(*v as u64, true);
                self.push(c);
            }
            Op::ConstI32(v) => {
                // Widen to i64 on push so the legacy-i64 envelope's
                // operand-stack invariant holds — every value in
                // flight is an `i64`.
                let i32_v = self.ctx.i32_type().const_int(*v as u32 as u64, false);
                let widened = self
                    .builder
                    .build_int_s_extend(i32_v, self.ctx.i64_type(), "i32_to_i64")
                    .map_err(|e| LlvmError::Codegen(format!("ConstI32 widen failed: {e}")))?;
                self.push(widened);
            }
            Op::ConstBool(b) => {
                // Same widening as ConstI32: the legacy envelope only
                // moves i64s on the stack, so Bool literals become an
                // i64 0/1.
                let c = self.ctx.i64_type().const_int(if *b { 1 } else { 0 }, false);
                self.push(c);
            }
            Op::LocalGet(idx) => {
                let p = self.func.get_nth_param(*idx).ok_or_else(|| {
                    LlvmError::Codegen(format!(
                        "LocalGet({idx}) out of range; function has {} param(s)",
                        self.func.count_params()
                    ))
                })?;
                let int_v = match p {
                    BasicValueEnum::IntValue(v) => v,
                    other => {
                        return Err(LlvmError::Codegen(format!(
                            "LocalGet({idx}) param is {other:?}, expected IntValue"
                        )));
                    }
                };
                self.push(int_v);
            }
            Op::Add(IrType::I64) => {
                let b = self.pop(ip, "Add")?;
                let a = self.pop(ip, "Add")?;
                let r = self
                    .builder
                    .build_int_add(a, b, "add")
                    .map_err(|e| LlvmError::Codegen(format!("Add build failed: {e}")))?;
                self.push(r);
            }
            Op::Sub(IrType::I64) => {
                let b = self.pop(ip, "Sub")?;
                let a = self.pop(ip, "Sub")?;
                let r = self
                    .builder
                    .build_int_sub(a, b, "sub")
                    .map_err(|e| LlvmError::Codegen(format!("Sub build failed: {e}")))?;
                self.push(r);
            }
            Op::Mul(IrType::I64) => {
                let b = self.pop(ip, "Mul")?;
                let a = self.pop(ip, "Mul")?;
                let r = self
                    .builder
                    .build_int_mul(a, b, "mul")
                    .map_err(|e| LlvmError::Codegen(format!("Mul build failed: {e}")))?;
                self.push(r);
            }
            Op::Return => {
                let v = self.pop(ip, "Return")?;
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| LlvmError::Codegen(format!("Return build failed: {e}")))?;
            }
            other => {
                return Err(LlvmError::Codegen(format!(
                    "unsupported op at ip={ip} (Phase A bootstrap): {other:?}"
                )));
            }
        }
        Ok(())
    }
}
