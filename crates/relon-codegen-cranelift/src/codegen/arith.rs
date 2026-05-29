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
use cranelift_codegen::ir::types::{I32, I64};
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

    /// `Op::Div(IrType::F64)` — IEEE-754 `fdiv`. No division-by-zero
    /// guard: Float division by zero yields ±inf / NaN per IEEE-754,
    /// matching the tree-walker (`lhs / rhs`) and the bytecode VM's
    /// `DivF64`. The integer `DivisionByZero` trap deliberately does not
    /// apply here.
    pub(super) fn emit_div_f64(&mut self) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().fdiv(a, b);
        self.push(r);
        Ok(())
    }

    /// Pop `[b, a]` and push `a fcmp(cc) b` widened to the IR's `Bool`
    /// slot (cranelift `i32`). Float comparisons use the *ordered*
    /// predicates so NaN operands compare `false` for `<`/`<=`/`>`/`>=`
    /// /`==` and `true` only for `!=`, matching Rust's `f64` operators
    /// the tree-walker + bytecode VM use.
    pub(super) fn emit_fcmp(&mut self, cc: FloatCC) -> Result<(), CraneliftError> {
        let b = self.pop()?;
        let a = self.pop()?;
        let r = self.builder.ins().fcmp(cc, a, b);
        let r = self.builder.ins().uextend(I32, r);
        self.push(r);
        Ok(())
    }
}
