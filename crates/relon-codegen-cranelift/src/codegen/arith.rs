//! Arithmetic + comparison lowering helpers for [`super::Codegen`].
//!
//! The `Op::Add` / `Op::Sub` / `Op::Mul` / `Op::Div` / `Op::Mod` /
//! `Op::BitAnd` arms and the six `Op::Eq` / `Op::Ne` / `Op::Lt` /
//! `Op::Le` / `Op::Gt` / `Op::Ge` arms all share the same `pop` /
//! `pop` / build / `push` skeleton. Each variant differs only in
//! cranelift instruction, overflow semantics, and the comparator
//! `IntCC`.
//!
//! Centralising the bodies here gives [`super::Codegen::emit_op`] a
//! flat dispatch table (one line per `Op::*` arm) and isolates the
//! signed-overflow / division-by-zero plumbing in one place. The
//! trap helpers (`cond_trap`) and the operand-stack helpers (`pop`,
//! `push`) remain on [`super::Codegen`] because they touch broader
//! state.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F64, I32, I64};
use cranelift_codegen::ir::InstBuilder;

use crate::error::CraneliftError;
use crate::sandbox::TrapKind;

impl<'a, 'b> super::Codegen<'a, 'b> {
    /// Pop `[b, a]` and push `a icmp(cc) b` widened to the IR's
    /// `Bool` slot (cranelift `i32`).
    pub(super) fn emit_cmp(&mut self, cc: IntCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().icmp(cc, a, b);
        // cranelift `icmp` produces an i8 in some versions, an i32 in
        // others; we normalise to i32 to match the IR's `Bool` slot.
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }

    /// Same as [`Self::emit_cmp`] but for operands already in the
    /// `I32` slot (no widening of operand before icmp).
    pub(super) fn emit_cmp_i32(&mut self, cc: IntCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().icmp(cc, a, b);
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }

    /// `Op::Add(IrType::I64)` — signed-overflow trap on overflow so
    /// the cranelift backend matches the tree-walker's strict
    /// semantics. The wasm-AOT backend wraps silently; cranelift
    /// differs deliberately to close the differential corpus.
    pub(super) fn emit_add_i64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let (r, of) = self.builder.ins().sadd_overflow(a, b);
        self.cond_trap(of, TrapKind::NumericOverflow);
        self.push(r);
        Ok(())
    }

    /// `Op::Sub(IrType::I64)` with signed-overflow trap.
    pub(super) fn emit_sub_i64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let (r, of) = self.builder.ins().ssub_overflow(a, b);
        self.cond_trap(of, TrapKind::NumericOverflow);
        self.push(r);
        Ok(())
    }

    /// `Op::Mul(IrType::I64)` with signed-overflow trap.
    pub(super) fn emit_mul_i64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let (r, of) = self.builder.ins().smul_overflow(a, b);
        self.cond_trap(of, TrapKind::NumericOverflow);
        self.push(r);
        Ok(())
    }

    /// `Op::Div(IrType::I64)`. Traps `DivisionByZero` when divisor
    /// is zero (guarded by `sandbox.div_check`); falls through to
    /// `sdiv` otherwise. The cond_trap helper routes through
    /// `raise_trap` + early return so the trap is observable through
    /// the typed `RuntimeError` channel rather than SIGFPE/SIGILL.
    pub(super) fn emit_div_i64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        if self.sandbox.div_check {
            let zero = self.builder.ins().iconst(I64, 0);
            let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
            self.cond_trap(cmp, TrapKind::DivisionByZero);
        }
        let r = self.builder.ins().sdiv(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Mod(IrType::I64)`. Mirrors [`Self::emit_div_i64`] but
    /// emits `srem`.
    pub(super) fn emit_mod_i64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        if self.sandbox.div_check {
            let zero = self.builder.ins().iconst(I64, 0);
            let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
            self.cond_trap(cmp, TrapKind::DivisionByZero);
        }
        let r = self.builder.ins().srem(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::BitAnd(IrType::I64)`.
    pub(super) fn emit_bitand_i64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().band(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Add(IrType::I32)` — wasm `i32.add` semantics. Used by
    /// stdlib bodies for pointer / length arithmetic against the
    /// linear-memory model. No overflow trap because wasm wraps.
    pub(super) fn emit_add_i32(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().iadd(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Sub(IrType::I32)` — wasm `i32.sub` semantics.
    pub(super) fn emit_sub_i32(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().isub(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Mul(IrType::I32)` — wasm `i32.mul` semantics.
    pub(super) fn emit_mul_i32(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().imul(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Div(IrType::I32)` with `DivisionByZero` guard.
    pub(super) fn emit_div_i32(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        if self.sandbox.div_check {
            let zero = self.builder.ins().iconst(I32, 0);
            let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
            self.cond_trap(cmp, TrapKind::DivisionByZero);
        }
        let r = self.builder.ins().sdiv(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Mod(IrType::I32)` with `DivisionByZero` guard.
    pub(super) fn emit_mod_i32(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        if self.sandbox.div_check {
            let zero = self.builder.ins().iconst(I32, 0);
            let cmp = self.builder.ins().icmp(IntCC::Equal, b, zero);
            self.cond_trap(cmp, TrapKind::DivisionByZero);
        }
        let r = self.builder.ins().srem(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::BitAnd(IrType::I32)`.
    pub(super) fn emit_bitand_i32(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().band(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Add(IrType::F64)` — IEEE-754 `fadd`. No overflow trap:
    /// Float arithmetic saturates to ±inf rather than trapping, matching
    /// the tree-walker (`lhs + rhs`) and the bytecode VM's `AddF64`.
    pub(super) fn emit_add_f64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().fadd(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Sub(IrType::F64)` — IEEE-754 `fsub`.
    pub(super) fn emit_sub_f64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().fsub(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Mul(IrType::F64)` — IEEE-754 `fmul`.
    pub(super) fn emit_mul_f64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().fmul(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::Div(IrType::F64)` — IEEE-754 `fdiv` with a `DivisionByZero`
    /// guard (gated by `sandbox.div_check`).
    ///
    /// The tree-walker oracle (`relon-evaluator::arithmetic::
    /// eval_numeric_division`) checks `right.as_f64() == 0.0` *before*
    /// the Int/Float split and raises `RuntimeError::DivisionByZero`, so
    /// a Float divide-by-zero is a runtime error rather than ±inf / NaN.
    /// A plain `fdiv` would diverge from the oracle on that operand, so
    /// we mirror the integer path (and the LLVM AOT `f64` div lowering,
    /// commit 4b59ebd1): compare the divisor against `0.0` with the
    /// *ordered-equal* predicate (`FloatCC::Equal`, which matches both
    /// `+0.0` and `-0.0` and declines for NaN, exactly like LLVM `OEQ`)
    /// and branch to the typed trap before the `fdiv`. The `cond_trap`
    /// helper routes through `raise_trap` so the trap surfaces as a
    /// `RuntimeError::DivisionByZero` rather than an IEEE infinity.
    pub(super) fn emit_div_f64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        if self.sandbox.div_check {
            let zero = self.builder.ins().f64const(0.0);
            let cmp = self.builder.ins().fcmp(FloatCC::Equal, b, zero);
            self.cond_trap(cmp, TrapKind::DivisionByZero);
        }
        let r = self.builder.ins().fdiv(a, b);
        self.push(r);
        Ok(())
    }

    /// `Op::ConvertI64ToF64` (#359) — pop one `I64` cranelift value and
    /// push its `fcvt_from_sint` (`sitofp`) widening as a native `F64`.
    /// Mirrors the tree-walker's `NumericValue::as_f64()` Int promotion
    /// (`value as f64`), feeding the mixed-type `fadd` / `fsub` / `fmul`
    /// / `fdiv` arms that lowering emits as `F64`.
    pub(super) fn emit_convert_i64_to_f64(&mut self) -> Result<(), CraneliftError> {
        let a = self.pop()?;
        let r = self.builder.ins().fcvt_from_sint(F64, a);
        self.push(r);
        Ok(())
    }

    /// Pop `[b, a]` and push `a fcmp(cc) b` widened to the IR's `Bool`
    /// slot (cranelift `i32`). Float *ordering* comparisons use the
    /// ordered predicates (`LessThan` / `LessThanOrEqual` /
    /// `GreaterThan` / `GreaterThanOrEqual`) so a NaN operand compares
    /// `false`, matching the tree-walker's `eval_numeric_comparison`
    /// (Rust native `f64` `<`/`<=`/`>`/`>=`). Equality (`==` / `!=`)
    /// must NOT route here — it has NaN-equals-NaN semantics handled by
    /// [`Self::emit_fcmp_eq`].
    pub(super) fn emit_fcmp(&mut self, cc: FloatCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().fcmp(cc, a, b);
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }

    /// Pop `[b, a]` and push the Float `==` (when `negate` is `false`)
    /// or `!=` (when `negate` is `true`) result, widened to the IR's
    /// `Bool` slot.
    ///
    /// `relon` compares `Value::Float` through `OrderedFloat`'s
    /// `PartialEq`, where `NaN == NaN` is **true** and `NaN != NaN` is
    /// **false** — the opposite of raw IEEE. A plain ordered `fcmp`
    /// (`FloatCC::Equal` / `FloatCC::NotEqual`) gets the finite and
    /// signed-zero cases right but reports `NaN == NaN` as false, so we
    /// OR in an explicit both-NaN test, mirroring the LLVM AOT lowering
    /// (`eq = OEQ(a,b) | (isnan(a) & isnan(b))`, `ne = !eq`):
    ///
    /// * `oeq = fcmp(Equal, a, b)` — ordered-equal (false for any NaN,
    ///   true for `+0.0 == -0.0`), matching LLVM `OEQ`.
    /// * `fcmp(Unordered, x, x)` is true iff `x` is NaN (a value is
    ///   unordered with itself only when it is NaN), the cranelift
    ///   analogue of LLVM `UNO(x, x)` / `x.is_nan()`.
    /// * `eq = oeq | (a_nan & b_nan)`, and `ne = eq XOR 1`.
    ///
    /// All boolean composition is done at the `I32` `Bool`-slot width so
    /// the result matches what the rest of the pipeline expects and does
    /// not depend on the native scalar `fcmp` result width.
    pub(super) fn emit_fcmp_eq(&mut self, negate: bool) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;

        let oeq = self.builder.ins().fcmp(FloatCC::Equal, a, b);
        let oeq = self.builder.ins().uextend(I32, oeq);

        let a_nan = self.builder.ins().fcmp(FloatCC::Unordered, a, a);
        let a_nan = self.builder.ins().uextend(I32, a_nan);
        let b_nan = self.builder.ins().fcmp(FloatCC::Unordered, b, b);
        let b_nan = self.builder.ins().uextend(I32, b_nan);
        let both_nan = self.builder.ins().band(a_nan, b_nan);

        let eq = self.builder.ins().bor(oeq, both_nan);
        let r = if negate {
            let one = self.builder.ins().iconst(I32, 1);
            self.builder.ins().bxor(eq, one)
        } else {
            eq
        };
        self.push(r);
        Ok(())
    }
}
