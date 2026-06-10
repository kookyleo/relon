//! `Op`-family: arithmetic + comparisons.
//!
//! Add/Sub/Mul/Div/Mod/BitAnd (I32/I64/F64), the i64->f64 convert, and
//! the integer/float comparison predicates. Behavior-preserving split
//! out of the monolithic emitter (Phase 0a); each `emit_*` is invoked
//! by the central `lower_op` dispatch in `super`.

use inkwell::values::{BasicMetadataValueEnum, IntValue};
use inkwell::{FloatPredicate, IntPredicate};

use relon_ir::ir::{F64UnaryOp, IrType};

use crate::error::LlvmError;

use super::*;

#[derive(Clone, Copy)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
}

impl BinOp {
    pub(crate) fn name(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "sdiv",
            BinOp::Mod => "srem",
            BinOp::BitAnd => "and",
        }
    }
}

impl<'ctx, 'b, 'cp> Emit<'ctx, 'b, 'cp> {
    pub(crate) fn emit_binop(
        &mut self,
        ip_hint: &str,
        ty: IrType,
        op: BinOp,
    ) -> Result<(), LlvmError> {
        let b = self.pop_int(ip_hint)?;
        let a = self.pop_int(ip_hint)?;

        // AOT-1 scalar Float slice: the operand stack carries f64 as
        // i64 bits tagged `IrType::F64`. Compute the arithmetic in the
        // float domain by bit-casting both operands to `double`, then
        // bit-cast the result back to i64 bits before pushing. The IR
        // guarantees homogeneous F64 operands (lowering rejects
        // mixed Int/Float), so no int<->float promotion is needed.
        //
        // The integer div-by-zero trap guard below assumes integer
        // operands (`build_int_compare` against an integer zero), so the
        // F64 path runs its own guard inside `emit_binop_f64`. The float
        // guard matches the tree-walker oracle, which raises
        // `DivisionByZero` for `x / 0.0` (see
        // `relon-evaluator::arithmetic::eval_numeric_division`) rather
        // than yielding IEEE ±inf.
        if ty == IrType::F64 {
            return self.emit_binop_f64(op, a, b);
        }

        // Phase E.2 sandbox parity: guard Div / Mod against a zero RHS
        // so the JIT raises a deterministic trap instead of leaving
        // LLVM's `sdiv` / `srem` to invoke UB (which on x86 surfaces
        // as a host-level SIGFPE that the host can't catch on stable
        // Rust). Emit an `if rhs == 0 { llvm.trap; unreachable } else
        // { ... }` skeleton and continue the division in the `else`
        // arm. The `unreachable` after `llvm.trap` is what tells LLVM
        // the trap path doesn't fall through.
        if matches!(op, BinOp::Div | BinOp::Mod) {
            let zero = b.get_type().const_zero();
            let cmp_name = self.next_name("divz_cmp");
            let is_zero = self
                .builder
                .build_int_compare(IntPredicate::EQ, b, zero, &cmp_name)
                .map_err(|e| LlvmError::Codegen(format!("{} divz cmp: {e}", op.name())))?;
            let trap_bb = self.ctx.append_basic_block(self.func, "div_by_zero_trap");
            let cont_bb = self.ctx.append_basic_block(self.func, "div_by_zero_ok");
            self.builder
                .build_conditional_branch(is_zero, trap_bb, cont_bb)
                .map_err(|e| LlvmError::Codegen(format!("{} divz branch: {e}", op.name())))?;
            // Trap block: call `llvm.trap` then `unreachable`. The
            // intrinsic is declared lazily; subsequent emits reuse the
            // declaration so the module ends up with at most one
            // `@llvm.trap` symbol regardless of how many guards fire.
            self.builder.position_at_end(trap_bb);
            self.emit_llvm_trap_call(op.name())?;
            self.builder
                .build_unreachable()
                .map_err(|e| LlvmError::Codegen(format!("{} divz unreachable: {e}", op.name())))?;
            // Continue normal codegen in the "ok" block.
            self.builder.position_at_end(cont_bb);
        }

        let name = self.next_name(op.name());
        let r = match op {
            BinOp::Add => self.builder.build_int_add(a, b, &name),
            BinOp::Sub => self.builder.build_int_sub(a, b, &name),
            BinOp::Mul => self.builder.build_int_mul(a, b, &name),
            BinOp::Div => self.builder.build_int_signed_div(a, b, &name),
            BinOp::Mod => self.builder.build_int_signed_rem(a, b, &name),
            BinOp::BitAnd => self.builder.build_and(a, b, &name),
        }
        .map_err(|e| LlvmError::Codegen(format!("{} build failed: {e}", op.name())))?;
        self.push(r, ty);
        Ok(())
    }

    /// #359: lower `Op::ConvertI64ToF64` — signed-int → float promotion.
    /// The operand-stack `I64` value is a *real* integer (not f64 bits),
    /// so we `sitofp` it to `double` and then bit-cast the `double` back
    /// to i64 bits, pushing the result tagged `F64` to match the
    /// AOT-1 "f64 rides as i64 bits" convention every later F64 op
    /// (`emit_binop_f64`, `StoreField(F64)`) expects.
    ///
    /// This statically reproduces the tree-walker's
    /// `NumericValue::as_f64()` Int promotion (`value as f64`): LLVM
    /// `sitofp i64 -> double` is the same round-to-nearest-even widening
    /// Rust's `i64 as f64` performs, so the resulting `f64::to_bits()`
    /// matches the oracle bit-for-bit.
    pub(crate) fn emit_convert_i64_to_f64(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let v = self.pop_int(ip_hint)?;
        let f64_t = self.ctx.f64_type();
        let f = self
            .builder
            .build_signed_int_to_float(v, f64_t, &self.next_name("sitofp"))
            .map_err(|e| LlvmError::Codegen(format!("ConvertI64ToF64 sitofp: {e}")))?;
        let bits = self
            .builder
            .build_bit_cast(f, self.ctx.i64_type(), &self.next_name("sitofp_bits"))
            .map_err(|e| LlvmError::Codegen(format!("ConvertI64ToF64 result bitcast: {e}")))?
            .into_int_value();
        self.push(bits, IrType::F64);
        Ok(())
    }

    /// Wave R7: declare-or-get a `double(double)` LLVM float intrinsic by
    /// name (e.g. `llvm.floor.f64`). Idempotent — repeated emits reuse
    /// the single module declaration, mirroring `declare_llvm_trap`.
    fn get_f64_unary_intrinsic(&self, name: &str) -> inkwell::values::FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(name) {
            return f;
        }
        let f64_t = self.ctx.f64_type();
        let fn_ty = f64_t.fn_type(&[f64_t.into()], false);
        self.module.add_function(name, fn_ty, None)
    }

    /// `Op::F64ToI64Sat` (Wave R7) — pop the operand-stack i64 bits, cast
    /// to `double`, and convert to a signed `i64` via the
    /// `llvm.fptosi.sat.i64.f64` saturating intrinsic. The saturating
    /// form (NOT the plain `fptosi`, whose out-of-range / NaN result is
    /// poison) reproduces Rust's `f64 as i64` cast that the tree-walk
    /// `floor` / `ceil` / `round` oracles use: clamp to `i64::MIN` /
    /// `i64::MAX` on overflow, `0` for `NaN`.
    pub(crate) fn emit_f64_to_i64_sat(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let bits = self.pop_int(ip_hint)?;
        let f64_t = self.ctx.f64_type();
        let i64_t = self.ctx.i64_type();
        let f = self
            .builder
            .build_bit_cast(bits, f64_t, &self.next_name("fptosi_a"))
            .map_err(|e| LlvmError::Codegen(format!("F64ToI64Sat bitcast: {e}")))?
            .into_float_value();
        // `llvm.fptosi.sat.i64.f64 : double -> i64`.
        let intr = {
            let name = "llvm.fptosi.sat.i64.f64";
            if let Some(g) = self.module.get_function(name) {
                g
            } else {
                let fn_ty = i64_t.fn_type(&[f64_t.into()], false);
                self.module.add_function(name, fn_ty, None)
            }
        };
        let args: [BasicMetadataValueEnum; 1] = [f.into()];
        let call_site = self
            .builder
            .build_call(intr, &args, &self.next_name("fptosi_sat"))
            .map_err(|e| LlvmError::Codegen(format!("F64ToI64Sat call: {e}")))?;
        let r = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(inkwell::values::BasicValueEnum::IntValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "F64ToI64Sat: intrinsic returned {other:?}, expected i64"
                )));
            }
        };
        self.push(r, IrType::I64);
        Ok(())
    }

    /// `Op::F64Unary(op)` (Wave R7) — pop the operand-stack i64 bits, cast
    /// to `double`, call the matching LLVM float intrinsic, and push the
    /// result back as i64 bits (the AOT-1 "f64 rides as i64 bits"
    /// convention). Each intrinsic's IEEE-754 semantics match the
    /// tree-walk oracle (`floor` / `ceil` / `round_ties_even` via
    /// `llvm.roundeven` / `sqrt` / `abs` via `llvm.fabs`).
    pub(crate) fn emit_f64_unary(
        &mut self,
        ip_hint: &str,
        op: F64UnaryOp,
    ) -> Result<(), LlvmError> {
        let bits = self.pop_int(ip_hint)?;
        let f64_t = self.ctx.f64_type();
        let f = self
            .builder
            .build_bit_cast(bits, f64_t, &self.next_name("funary_a"))
            .map_err(|e| LlvmError::Codegen(format!("F64Unary bitcast: {e}")))?
            .into_float_value();
        let intr_name = match op {
            F64UnaryOp::Floor => "llvm.floor.f64",
            F64UnaryOp::Ceil => "llvm.ceil.f64",
            F64UnaryOp::Nearest => "llvm.roundeven.f64",
            F64UnaryOp::Sqrt => "llvm.sqrt.f64",
            F64UnaryOp::Abs => "llvm.fabs.f64",
        };
        let intr = self.get_f64_unary_intrinsic(intr_name);
        let args: [BasicMetadataValueEnum; 1] = [f.into()];
        let call_site = self
            .builder
            .build_call(intr, &args, &self.next_name("funary"))
            .map_err(|e| LlvmError::Codegen(format!("F64Unary call: {e}")))?;
        let rf = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(inkwell::values::BasicValueEnum::FloatValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "F64Unary: intrinsic returned {other:?}, expected double"
                )));
            }
        };
        let out_bits = self
            .builder
            .build_bit_cast(rf, self.ctx.i64_type(), &self.next_name("funary_bits"))
            .map_err(|e| LlvmError::Codegen(format!("F64Unary result bitcast: {e}")))?
            .into_int_value();
        self.push(out_bits, IrType::F64);
        Ok(())
    }

    /// `Op::F64Pow` — pop `[exp, base]` operand-stack i64 bits, cast both
    /// to `double`, call the `llvm.pow.f64` intrinsic, and push the
    /// result back as i64 bits. Never traps — the tree-walk oracle
    /// (`to_f64_val(a).powf(to_f64_val(b))`) returns `inf` / NaN per
    /// IEEE-754 rather than raising. The native MCJIT path resolves the
    /// intrinsic's `pow` libcall against process libm in-process (the
    /// same `pow` that Rust's `f64::powf` calls); the wasm32 path lowers
    /// it to an undefined `pow` env import the host binds to
    /// `f64::powf` — identical bits on all legs.
    pub(crate) fn emit_f64_pow(&mut self, ip_hint: &str) -> Result<(), LlvmError> {
        let exp_bits = self.pop_int(ip_hint)?;
        let base_bits = self.pop_int(ip_hint)?;
        let f64_t = self.ctx.f64_type();
        let base = self
            .builder
            .build_bit_cast(base_bits, f64_t, &self.next_name("fpow_a"))
            .map_err(|e| LlvmError::Codegen(format!("F64Pow base bitcast: {e}")))?
            .into_float_value();
        let exp = self
            .builder
            .build_bit_cast(exp_bits, f64_t, &self.next_name("fpow_b"))
            .map_err(|e| LlvmError::Codegen(format!("F64Pow exp bitcast: {e}")))?
            .into_float_value();
        // `llvm.pow.f64 : (double, double) -> double`.
        let intr = {
            let name = "llvm.pow.f64";
            if let Some(g) = self.module.get_function(name) {
                g
            } else {
                let fn_ty = f64_t.fn_type(&[f64_t.into(), f64_t.into()], false);
                self.module.add_function(name, fn_ty, None)
            }
        };
        let args: [BasicMetadataValueEnum; 2] = [base.into(), exp.into()];
        let call_site = self
            .builder
            .build_call(intr, &args, &self.next_name("fpow"))
            .map_err(|e| LlvmError::Codegen(format!("F64Pow call: {e}")))?;
        let rf = match call_site.try_as_basic_value() {
            inkwell::values::ValueKind::Basic(inkwell::values::BasicValueEnum::FloatValue(v)) => v,
            other => {
                return Err(LlvmError::Codegen(format!(
                    "F64Pow: intrinsic returned {other:?}, expected double"
                )));
            }
        };
        let out_bits = self
            .builder
            .build_bit_cast(rf, self.ctx.i64_type(), &self.next_name("fpow_bits"))
            .map_err(|e| LlvmError::Codegen(format!("F64Pow result bitcast: {e}")))?
            .into_int_value();
        self.push(out_bits, IrType::F64);
        Ok(())
    }

    /// AOT-1: lower an `F64` binary op. `a` / `b` are the operand-stack
    /// i64 bit patterns; we bit-cast to `double`, run the matching
    /// `build_float_*`, and bit-cast the result back to i64 bits so the
    /// virtual stack stays integer-typed (Option B — no enum StackVal
    /// rewrite).
    ///
    /// #362: `Mod` lowers to `frem`, which on every supported target
    /// lowers to the C `fmod` (truncated remainder, sign of the
    /// dividend) — bit-identical to Rust's `f64 %`, i.e. the
    /// tree-walker's `a.as_f64() % b.as_f64()`.
    ///
    /// Both `Div` and `Mod` carry a float-zero trap guard: the
    /// tree-walker oracle (`eval_numeric_division`) raises
    /// `DivisionByZero` whenever the divisor compares equal to `0.0`
    /// *before* the `/` or `%` runs (which `OEQ` matches for both
    /// `+0.0` and `-0.0`, and declines for `NaN`), so the JIT must trap
    /// on the same operands rather than producing IEEE ±inf (`/`) or
    /// `NaN` (`%`).
    pub(crate) fn emit_binop_f64(
        &mut self,
        op: BinOp,
        a: IntValue<'ctx>,
        b: IntValue<'ctx>,
    ) -> Result<(), LlvmError> {
        let f64_t = self.ctx.f64_type();
        let af = self
            .builder
            .build_bit_cast(a, f64_t, &self.next_name("fbin_a"))
            .map_err(|e| LlvmError::Codegen(format!("{} f64 lhs bitcast: {e}", op.name())))?
            .into_float_value();
        let bf = self
            .builder
            .build_bit_cast(b, f64_t, &self.next_name("fbin_b"))
            .map_err(|e| LlvmError::Codegen(format!("{} f64 rhs bitcast: {e}", op.name())))?
            .into_float_value();
        if matches!(op, BinOp::Div | BinOp::Mod) {
            let zero = f64_t.const_zero();
            let cmp_name = self.next_name("fdivz_cmp");
            let is_zero = self
                .builder
                .build_float_compare(FloatPredicate::OEQ, bf, zero, &cmp_name)
                .map_err(|e| LlvmError::Codegen(format!("f64 divz cmp: {e}")))?;
            let trap_bb = self.ctx.append_basic_block(self.func, "fdiv_by_zero_trap");
            let cont_bb = self.ctx.append_basic_block(self.func, "fdiv_by_zero_ok");
            self.builder
                .build_conditional_branch(is_zero, trap_bb, cont_bb)
                .map_err(|e| LlvmError::Codegen(format!("f64 divz branch: {e}")))?;
            self.builder.position_at_end(trap_bb);
            self.emit_llvm_trap_call(op.name())?;
            self.builder
                .build_unreachable()
                .map_err(|e| LlvmError::Codegen(format!("f64 divz unreachable: {e}")))?;
            self.builder.position_at_end(cont_bb);
        }
        let name = self.next_name(op.name());
        let rf = match op {
            BinOp::Add => self.builder.build_float_add(af, bf, &name),
            BinOp::Sub => self.builder.build_float_sub(af, bf, &name),
            BinOp::Mul => self.builder.build_float_mul(af, bf, &name),
            BinOp::Div => self.builder.build_float_div(af, bf, &name),
            // #362: `frem` lowers to `fmod` (truncated remainder, sign
            // of the dividend) — bit-identical to Rust's `f64 %`.
            BinOp::Mod => self.builder.build_float_rem(af, bf, &name),
            BinOp::BitAnd => {
                return Err(LlvmError::Codegen(format!(
                    "{} not defined for F64 operands",
                    op.name()
                )));
            }
        }
        .map_err(|e| LlvmError::Codegen(format!("{} f64 build failed: {e}", op.name())))?;
        let bits = self
            .builder
            .build_bit_cast(rf, self.ctx.i64_type(), &self.next_name("fbin_bits"))
            .map_err(|e| LlvmError::Codegen(format!("{} f64 result bitcast: {e}", op.name())))?
            .into_int_value();
        self.push(bits, IrType::F64);
        Ok(())
    }

    pub(crate) fn emit_cmp(
        &mut self,
        ip_hint: &str,
        operand_ty: IrType,
        pred: IntPredicate,
    ) -> Result<(), LlvmError> {
        // Pop in the order [b, a] — the deepest operand is the first
        // push (lhs of the comparison).
        let b = self.pop_int(ip_hint)?;
        let a = self.pop_int(ip_hint)?;
        // AOT-1 scalar Float slice: an F64 comparison reinterprets the
        // i64-bits operands as `double`. The predicate choice tracks the
        // tree-walker oracle exactly, NOT raw IEEE:
        //
        // * Ordering (`< <= > >=`) routes to the ORDERED predicates
        //   (OLT/OLE/OGT/OGE). These are false when either operand is
        //   NaN, matching the evaluator's `eval_numeric_comparison`
        //   (Rust native `f64` `<` etc.).
        // * Equality (`==` / `!=`) follows `Value`'s `PartialEq`, which
        //   compares `Value::Float` through `OrderedFloat`: `NaN == NaN`
        //   is *true* and `-0.0 == 0.0` is *true*. IEEE `OEQ` gets the
        //   zero case right but says `NaN == NaN` is false, so we OR in
        //   an explicit both-NaN test: `eq = OEQ(a,b) | (isnan(a) &
        //   isnan(b))`, and `ne = !eq`.
        let result_i1 = if operand_ty == IrType::F64 {
            let f64_t = self.ctx.f64_type();
            let af = self
                .builder
                .build_bit_cast(a, f64_t, &self.next_name("fcmp_a"))
                .map_err(|e| LlvmError::Codegen(format!("Cmp f64 lhs bitcast: {e}")))?
                .into_float_value();
            let bf = self
                .builder
                .build_bit_cast(b, f64_t, &self.next_name("fcmp_b"))
                .map_err(|e| LlvmError::Codegen(format!("Cmp f64 rhs bitcast: {e}")))?
                .into_float_value();
            match pred {
                IntPredicate::EQ | IntPredicate::NE => {
                    let oeq = self
                        .builder
                        .build_float_compare(FloatPredicate::OEQ, af, bf, &self.next_name("foeq"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 oeq: {e}")))?;
                    // `UNO(x, x)` is true iff `x` is NaN (the only way a
                    // value is unordered with itself).
                    let a_nan = self
                        .builder
                        .build_float_compare(FloatPredicate::UNO, af, af, &self.next_name("fanan"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 lhs isnan: {e}")))?;
                    let b_nan = self
                        .builder
                        .build_float_compare(FloatPredicate::UNO, bf, bf, &self.next_name("fbnan"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 rhs isnan: {e}")))?;
                    let both_nan = self
                        .builder
                        .build_and(a_nan, b_nan, &self.next_name("fbothnan"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 both-nan and: {e}")))?;
                    let eq = self
                        .builder
                        .build_or(oeq, both_nan, &self.next_name("feq"))
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 eq or: {e}")))?;
                    if matches!(pred, IntPredicate::EQ) {
                        eq
                    } else {
                        self.builder
                            .build_not(eq, &self.next_name("fne"))
                            .map_err(|e| LlvmError::Codegen(format!("Cmp f64 ne not: {e}")))?
                    }
                }
                ord => {
                    let fpred = match ord {
                        IntPredicate::SLT => FloatPredicate::OLT,
                        IntPredicate::SLE => FloatPredicate::OLE,
                        IntPredicate::SGT => FloatPredicate::OGT,
                        IntPredicate::SGE => FloatPredicate::OGE,
                        other => {
                            return Err(LlvmError::Codegen(format!(
                                "Cmp f64: unsupported predicate {other:?}"
                            )));
                        }
                    };
                    let name = self.next_name("fcmp");
                    self.builder
                        .build_float_compare(fpred, af, bf, &name)
                        .map_err(|e| LlvmError::Codegen(format!("Cmp f64 build failed: {e}")))?
                }
            }
        } else {
            // Phase B keeps every integer comparison signed (matches
            // what the IR producer emits for `Lt` / `Le` / `Gt` / `Ge`).
            // `Eq` / `Ne` are signedness-agnostic at the LLVM level, so
            // the producer's predicate flows through unchanged.
            let name = self.next_name("cmp");
            self.builder
                .build_int_compare(pred, a, b, &name)
                .map_err(|e| LlvmError::Codegen(format!("Cmp build failed: {e}")))?
        };
        // The IR's virtual stack wants a `Bool` (i32 slot). Widen the
        // i1 to i32 so the rest of the pipeline (StoreField for Bool
        // returns, BrIf for control flow) sees the canonical width.
        let name_zext = self.next_name("cmp_zext");
        let widened = self
            .builder
            .build_int_z_extend(result_i1, self.ctx.i32_type(), &name_zext)
            .map_err(|e| LlvmError::Codegen(format!("Cmp zext: {e}")))?;
        self.push(widened, IrType::Bool);
        Ok(())
    }
}
